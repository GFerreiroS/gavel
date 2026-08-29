#!/usr/bin/env python3
"""Reconcile catalogs.json against what is actually trading on the auction house.

Also resolves the bonus ids that per-realm gear carries, which is a separate
job with separate sources -- see `--realm` below.

The catalog decides what gets collected, so a category that claims to track
"everything of this expansion" is a claim about a live data set, not about a
file. Every patch adds items; nobody notices a gap by reading JSON. This
script asks Blizzard instead: pull the commodity snapshot, look up each item's
class and subclass, and compare that to what the catalog holds.

    python3 scripts/catalog-sync.py                 # report drift, change nothing
    python3 scripts/catalog-sync.py --write         # rewrite the generated kinds

Two kinds are *generated* and may be rewritten: enchants and gems. Their
grouping (equipment slot, gem subclass) comes straight from Blizzard, so there
is nothing a human adds that a rewrite would destroy.

Two kinds are *editorial* and are only ever reported on: consumables and
reagents. Their `audience`, `stat` and `profession` are judgements the API
cannot make -- "mana potions are caster-only" is not in the item data -- and a
rewrite would silently discard them. When this reports drift there, the fix is
to edit catalogs.json by hand.

Requires BLIZZARD_CLIENT_ID and BLIZZARD_CLIENT_SECRET, read from .env. The
snapshot is 30 MB and the item endpoint is one request per item, so responses
are cached under target/catalog-sync/ between runs.
"""

import argparse
import collections
import json
import pathlib
import re
import sys
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor

try:
    import requests
except ImportError:
    sys.exit("this script needs `requests` (pip install requests)")

ROOT = pathlib.Path(__file__).resolve().parent.parent
CATALOGS = ROOT / "crates/app-core/src/market/catalogs.json"
CACHE = ROOT / "target/catalog-sync"

OAUTH_URL = "https://oauth.battle.net/token"

# --- gear bonus ids ------------------------------------------------------
# A gear auction says only which *bonus ids* it carries. Blizzard publishes no
# meaning for them, so two other sources fill the gap, each doing the half it
# is good at:
#
#   * SimulationCraft's generated item-bonus table says what *kind* each id is.
#     Type 34 is an upgrade level, and that classification is exact -- it picks
#     out the eight ids we see and nothing else. The branch below is built from
#     the same game build the API serves.
#   * Wowhead's tooltip renderer turns a bonus combination into the words the
#     game shows: "Item Level 305", "Upgrade Level: Hero 1/6", "Prismatic
#     Socket". That is the half simc cannot give without joining several more
#     tables.
#
# Neither is called at runtime. What they produce is written into catalogs.json
# and committed, so the app depends on a reviewed file and nothing else.
SIMC_BONUS_URL = (
    "https://raw.githubusercontent.com/simulationcraft/simc/midnight"
    "/engine/dbc/generated/item_bonus.inc"
)
WOWHEAD_TOOLTIP = "https://nether.wowhead.com/tooltip/item/{item}?bonus={bonus}&dataEnv=1&locale=0"

# simc bonus types we care about. 34 is an upgrade level, and picks out
# exactly the eight ids the auctions carry. 4 names an upgrade track ("Heroic")
# and 16 is the bind flag -- both draw a tooltip line without being something
# a buyer is choosing, so they are kept out of the modifier list.
UPGRADE_TYPE = 34
STRUCTURAL_TYPES = (4, 16)

# Item classes, as Blizzard names them, mapped onto our kinds. A class we do
# not map is a category we have decided not to track: Housing, Glyph,
# Miscellaneous and Quest are all deliberate omissions, not oversights.
CLASS_KINDS = {
    "Tradeskill": "reagent",
    "Item Enhancement": "enchant",
    "Gem": "gem",
    "Consumable": "consumable",
}

# Kinds this script may rewrite. The rest are reported and left alone.
GENERATED = ("enchant", "gem")

# Kinds that are not commodities. This script reads the commodity endpoint, so
# it can say nothing about them: they live in per-realm auction houses and are
# reconciled, if at all, against those.
PER_REALM = ("boe", "recipe")

# Gems are tracked at rare quality only -- the "Flawless" tier. Uncommon is
# the levelling cut nobody raids in, and epic gems are a handful of one-off
# items on a different market entirely.
GEM_QUALITY = "RARE"

# A gem's subclass is the stat it grants. The four single-stat cuts carry it
# into the catalog, which is what puts them at the top of the page; "Multiple
# Stats" and "Other" have no single stat and are left unset.
GEM_STATS = {
    "Critical Strike": "crit",
    "Haste": "haste",
    "Mastery": "mastery",
    "Versatility": "versatility",
}

# Blizzard's subclass names for Item Enhancement are equipment slots. Kept as
# an explicit map rather than slugified on the fly, so a new slot appearing
# upstream is a visible failure here rather than a silent new group in the UI.
SLOTS = {
    "Head": "head",
    "Shoulder": "shoulder",
    "Cloak": "cloak",
    "Chest": "chest",
    "Legs": "legs",
    "Feet": "feet",
    "Finger": "finger",
    "Weapon": "weapon",
    "Two-Handed Weapon": "two_handed_weapon",
}


def credentials():
    """Read the client credentials out of .env. Never printed, never logged."""
    env = {}
    path = ROOT / ".env"
    if not path.exists():
        sys.exit(".env not found; copy .env.example and fill in the credentials")
    for line in path.read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            key, value = line.split("=", 1)
            env[key.strip()] = value.strip()
    try:
        return env["BLIZZARD_CLIENT_ID"], env["BLIZZARD_CLIENT_SECRET"]
    except KeyError:
        sys.exit("BLIZZARD_CLIENT_ID / BLIZZARD_CLIENT_SECRET missing from .env")


def token(client_id, client_secret):
    response = requests.post(
        OAUTH_URL,
        data={"grant_type": "client_credentials"},
        auth=(client_id, client_secret),
        timeout=30,
    )
    response.raise_for_status()
    return response.json()["access_token"]


def commodities(session, region, bearer, refresh):
    """Every commodity currently listed, as {item_id: units for sale}."""
    path = CACHE / f"commodities-{region}.json"
    if refresh or not path.exists():
        response = session.get(
            f"https://{region}.api.blizzard.com/data/wow/auctions/commodities",
            headers={"Authorization": f"Bearer {bearer}"},
            params={"namespace": f"dynamic-{region}"},
            timeout=300,
        )
        response.raise_for_status()
        path.write_bytes(response.content)
    volume = collections.Counter()
    for auction in json.loads(path.read_text())["auctions"]:
        volume[auction["item"]["id"]] += auction["quantity"]
    return volume


def english(value):
    """One string out of Blizzard's `{locale: text}` map.

    English only, and only here: the catalog stores English names as an
    identifier for humans reading the JSON. What the site *renders* comes from
    the tooltip cache in every language, so nothing user-facing depends on it.
    """
    if value is None:
        return None
    if isinstance(value, str):
        return value
    return value.get("en_GB") or value.get("en_US") or next(iter(value.values()), None)


def fetch_json(session, url, bearer, params, path):
    """A cached GET. 404 is cached too: a missing item stays missing."""
    if path.exists():
        return json.loads(path.read_text())
    for attempt in range(3):
        response = session.get(
            url, headers={"Authorization": f"Bearer {bearer}"}, params=params, timeout=30
        )
        if response.status_code == 200:
            path.write_bytes(response.content)
            return response.json()
        if response.status_code == 404:
            path.write_text('{"missing": true}')
            return {"missing": True}
        # 429 and 5xx: back off and try again. The budget is 36k/hour, so this
        # is a hiccup rather than a wall.
        time.sleep(1 + attempt)
    sys.exit(f"giving up on {url}")


def item_details(session, region, bearer, ids, workers):
    """Class, subclass, quality, item level and icon for each id."""
    items = CACHE / "items"
    media = CACHE / "media"
    items.mkdir(parents=True, exist_ok=True)
    media.mkdir(parents=True, exist_ok=True)
    host = f"https://{region}.api.blizzard.com"
    namespace = {"namespace": f"static-{region}"}

    def one(item_id):
        raw = fetch_json(
            session,
            f"{host}/data/wow/item/{item_id}",
            bearer,
            namespace,
            items / f"{item_id}.json",
        )
        if raw.get("missing"):
            return None
        icon = fetch_json(
            session,
            f"{host}/data/wow/media/item/{item_id}",
            bearer,
            namespace,
            media / f"{item_id}.json",
        )
        return {
            "id": item_id,
            "name": english(raw.get("name")),
            "item_class": english((raw.get("item_class") or {}).get("name")),
            "subclass": english((raw.get("item_subclass") or {}).get("name")),
            "quality": (raw.get("quality") or {}).get("type"),
            # The rank discriminator: ids are not ordered by rank, item level
            # is. Same rule the hand-written half of the catalog followed.
            "level": raw.get("level") or 0,
            "icon": icon_slug(icon),
        }

    with ThreadPoolExecutor(max_workers=workers) as pool:
        return [row for row in pool.map(one, ids) if row]


def icon_slug(media):
    """`7548915.jpg` out of a media payload. The host belongs to the template."""
    for asset in media.get("assets", []):
        if asset.get("key") == "icon":
            return asset.get("value", "").rsplit("/", 1)[-1] or None
    return None


def simc_bonus_types(session, refresh):
    """{bonus id: {simc type, ...}} from the generated table, cached on disk."""
    path = CACHE / "item_bonus.inc"
    if refresh or not path.exists():
        response = session.get(SIMC_BONUS_URL, timeout=120)
        response.raise_for_status()
        path.write_bytes(response.content)
    types = collections.defaultdict(set)
    entry = re.compile(r"\s*\{\s*\d+,\s*(\d+),\s*(\d+),")
    for line in path.read_text().splitlines():
        found = entry.match(line)
        if found:
            types[int(found.group(1))].add(int(found.group(2)))
    build = path.read_text(errors="ignore").split("\n", 1)[0].strip("/ ")
    return types, build


def wowhead_lines(item, bonus):
    """The tooltip the game would draw, as plain lines."""
    request = urllib.request.Request(
        WOWHEAD_TOOLTIP.format(item=item, bonus=bonus),
        headers={"User-Agent": "Mozilla/5.0"},
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        payload = json.load(response)
    text = re.sub(r"<[^>]+>", "\n", payload.get("tooltip", "")).replace("\xa0", " ")
    return [line.strip() for line in text.split("\n") if line.strip()]


def resolve_bonuses(session, listings, wanted, refresh):
    """Turn the bonus ids seen in real auctions into item levels and names.

    Returns (levels, modifiers). `levels` is keyed by the upgrade bonus id --
    which is enough on its own, because each one belongs to exactly one track
    -- and `modifiers` names the optional extras a piece may carry.
    """
    types, build = simc_bonus_types(session, refresh)
    print(f"simc {build}")

    # What the auctions actually contain, per item, so a tooltip can be asked
    # for a combination that really exists rather than an invented one.
    seen = collections.defaultdict(set)
    for auction in listings:
        item = auction["item"]["id"]
        if item in wanted:
            seen[item].add(tuple(sorted(auction["item"].get("bonus_lists", []))))

    upgrades = {}
    extras = {}
    for item, combos in sorted(seen.items()):
        for combo in sorted(combos):
            rank = next((b for b in combo if UPGRADE_TYPE in types.get(b, ())), None)
            if rank is None or rank in upgrades:
                continue
            lines = wowhead_lines(item, ":".join(str(b) for b in combo))
            level = next(
                (l for a, l in zip(lines, lines[1:]) if a == "Item Level"), None
            )
            # The game draws this as two runs: "Upgrade Level: Hero" then
            # "1/6". Rejoined here so the catalog holds "Hero 1/6".
            track = next(
                (
                    f'{a.split(":", 1)[1].strip()} {l.strip()}'
                    for a, l in zip(lines, lines[1:])
                    if a.startswith("Upgrade Level")
                ),
                "",
            )
            if level is None:
                continue
            upgrades[rank] = {
                "item_level": int(level),
                "upgrade": " ".join(track.split()),
            }
            time.sleep(0.2)

    # Name the optional bonuses by asking what each one adds to a tooltip.
    baseline_item, baseline_combo = next(
        ((i, c) for i, combos in sorted(seen.items()) for c in sorted(combos)),
        (None, None),
    )
    if baseline_item is not None:
        core = [b for b in baseline_combo if UPGRADE_TYPE in types.get(b, ())]
        base = set(wowhead_lines(baseline_item, ":".join(str(b) for b in core)))
        candidates = {b for combos in seen.values() for c in combos for b in c}
        for bonus in sorted(candidates):
            kinds = types.get(bonus, ())
            if UPGRADE_TYPE in kinds or any(t in kinds for t in STRUCTURAL_TYPES):
                continue
            added = [
                l
                for l in wowhead_lines(
                    baseline_item, ":".join(str(b) for b in core + [bonus])
                )
                if l not in base
            ]
            # An id that draws nothing is an "absence" marker -- "no socket",
            # "no tertiary". Recording those would be reporting a negative.
            if added:
                # "50 Avoidance" -> "Avoidance": the amount scales with the
                # item level, so it belongs to a listing rather than to the
                # bonus id, and storing one item's number would be a lie on
                # every other item.
                extras[bonus] = re.sub(r"^[\d,]+\s*", "", added[0])
            time.sleep(0.2)
    return upgrades, extras


def entries(rows, kind):
    """Catalog entries for one kind, ranks grouped by name and ordered by level."""
    by_name = collections.defaultdict(list)
    for row in rows:
        by_name[row["name"]].append(row)

    out = []
    for name, group in sorted(by_name.items()):
        group.sort(key=lambda r: (r["level"], r["id"]))
        entry = {
            "name": name,
            "category": kind,
            "audience": "common",
            "kind": kind,
        }
        if kind == "gem":
            stat = GEM_STATS.get(group[0]["subclass"])
            if stat:
                entry["stat"] = stat
        if kind == "enchant":
            slot = SLOTS.get(group[0]["subclass"])
            if slot is None:
                sys.exit(
                    f"unknown equipment slot {group[0]['subclass']!r} on {name!r}; "
                    "add it to SLOTS here and to Slot in catalog.rs"
                )
            entry["slot"] = slot
        entry["ranks"] = [
            {"rank": rank, "item_id": row["id"]} for rank, row in enumerate(group, 1)
        ]
        icon = next((row["icon"] for row in group if row["icon"]), None)
        if icon:
            entry["icon"] = icon
        out.append(entry)
    return out


def classify(rows, floor):
    """Split the discovered items into our kinds, dropping what we do not track."""
    kinds = collections.defaultdict(list)
    for row in rows:
        if row["id"] < floor:
            continue
        kind = CLASS_KINDS.get(row["item_class"])
        if kind is None:
            continue
        if kind == "gem" and row["quality"] != GEM_QUALITY:
            continue
        kinds[kind].append(row)
    return kinds


def tracked(catalog):
    """{kind: {item_id}} as the catalog currently stands."""
    out = collections.defaultdict(set)
    for item in catalog["items"]:
        kind = item.get("kind", "consumable")
        for rank in item["ranks"]:
            out[kind].add(rank["item_id"])
    return out


def report(kind, held, found, names):
    added = sorted(found - held)
    removed = sorted(held - found)
    label = "generated" if kind in GENERATED else "editorial"
    if not added and not removed:
        print(f"  {kind:<10} {len(held):>4} ids  in sync ({label})")
        return False
    print(f"  {kind:<10} {len(held):>4} ids  +{len(added)} -{len(removed)} ({label})")
    for item_id in added[:20]:
        print(f"      + {item_id}  {names.get(item_id, '?')}")
    if len(added) > 20:
        print(f"      + … {len(added) - 20} more")
    for item_id in removed[:20]:
        print(f"      - {item_id}  no longer listed or no longer this expansion")
    return True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--region", default="eu", help="commodity market to read")
    parser.add_argument(
        "--floor",
        type=int,
        default=None,
        help="lowest item id counted as this expansion "
        "(default: the lowest the catalog already tracks)",
    )
    parser.add_argument(
        "--write",
        action="store_true",
        help=f"rewrite the generated kinds ({', '.join(GENERATED)}) in catalogs.json",
    )
    parser.add_argument(
        "--refresh", action="store_true", help="re-fetch the snapshot instead of the cache"
    )
    parser.add_argument(
        "--realm",
        default=None,
        metavar="REGION:ID",
        help="resolve the gear bonus ids seen on this connected realm "
        "(e.g. eu:1403) and write them into the catalog",
    )
    parser.add_argument("--workers", type=int, default=12)
    args = parser.parse_args()

    CACHE.mkdir(parents=True, exist_ok=True)
    document = json.loads(CATALOGS.read_text())
    catalog = next((c for c in document["catalogs"] if c["status"] == "active"), None)
    if catalog is None:
        sys.exit("no active catalog; nothing to reconcile")

    held = tracked(catalog)
    floor = args.floor or min(r["item_id"] for i in catalog["items"] for r in i["ranks"])

    bearer = token(*credentials())
    session = requests.Session()
    volume = commodities(session, args.region, bearer, args.refresh)
    candidates = sorted(item_id for item_id in volume if item_id >= floor)
    print(
        f"{catalog['id']}: {len(volume)} commodities listed in {args.region.upper()}, "
        f"{len(candidates)} at or above item id {floor}"
    )

    rows = item_details(session, args.region, bearer, candidates, args.workers)
    kinds = classify(rows, floor)
    names = {row["id"]: row["name"] for row in rows}

    drifted = False
    for kind in sorted(set(kinds) | set(held)):
        if kind in PER_REALM:
            print(f"  {kind:<10} {len(held.get(kind, set())):>4} ids  per-realm, not checked here")
            continue
        found = {row["id"] for row in kinds.get(kind, [])}
        drifted |= report(kind, held.get(kind, set()), found, names)

    if args.realm:
        region, realm_id = args.realm.split(":")
        path = CACHE / f"realm-{region}-{realm_id}.json"
        if args.refresh or not path.exists():
            response = session.get(
                f"https://{region}.api.blizzard.com/data/wow/connected-realm/{realm_id}/auctions",
                headers={"Authorization": f"Bearer {bearer}"},
                params={"namespace": f"dynamic-{region}"},
                timeout=300,
            )
            response.raise_for_status()
            path.write_bytes(response.content)
        per_realm = {
            rank["item_id"]
            for item in catalog["items"]
            if item.get("kind") in PER_REALM
            for rank in item["ranks"]
        }
        listings = json.loads(path.read_text())["auctions"]
        levels, modifiers = resolve_bonuses(session, listings, per_realm, args.refresh)
        print(f"\n{len(levels)} item levels and {len(modifiers)} modifiers resolved:")
        for bonus, level in sorted(levels.items(), key=lambda kv: kv[1]["item_level"]):
            print(f'  bonus {bonus:>6}  ilvl {level["item_level"]:>4}  {level["upgrade"]}')
        for bonus, name in sorted(modifiers.items()):
            print(f"  bonus {bonus:>6}  {name}")
        if args.write:
            catalog["item_levels"] = {str(k): v for k, v in sorted(levels.items())}
            catalog["modifiers"] = {str(k): v for k, v in sorted(modifiers.items())}
            CATALOGS.write_text(json.dumps(document, indent=2, ensure_ascii=False) + "\n")
            print(f"\nwrote them to {CATALOGS.relative_to(ROOT)}")
        else:
            print("\nrun again with --write to store them")
        return

    if not args.write:
        if drifted:
            print("\nrun again with --write to rewrite the generated kinds")
        return

    keep = [i for i in catalog["items"] if i.get("kind", "consumable") not in GENERATED]
    generated = []
    for kind in GENERATED:
        generated += entries(kinds.get(kind, []), kind)
    catalog["items"] = keep + generated
    CATALOGS.write_text(json.dumps(document, indent=2, ensure_ascii=False) + "\n")
    print(f"\nwrote {len(generated)} generated entries to {CATALOGS.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
