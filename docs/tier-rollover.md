# Running a patch or a tier rollover

This is the operation CLAUDE.md §16's Phase 9 makes a **data** change: a new
patch, or a new raid tier, becomes browsable and starts collecting without a
route, a template, a statistic or a migration moving. If you find yourself
writing code to do any of this, something has drifted and the thing to fix is
the drift.

Two operations, and they are not the same size.

---

## A. A patch shipped (no new tier)

A patch is a boundary in the history. It segments prices —
`Window::Patch("12.1")` is materialised for every market — and it gets a page
at `/wow/archive/{expansion}/{patch}` because the hierarchy is read off the
catalogue.

1. **Add the patch** to the active catalogue in
   `crates/app-core/src/market/catalogs.json`:

   ```json
   {"patch": "12.1.5", "name": "Whatever it is called", "started": "2026-12-08"}
   ```

   `started` is the day it went live, `YYYY-MM-DD`, in UTC.

2. **Bump `catalog_version`.** A statistic materialised under one definition
   of a market is not the same as one materialised under another, and the
   version is how a row says which it was.

3. **Reconcile the catalogue against what is actually trading.** A patch adds
   items, and nobody spots a gap by reading JSON:

   ```bash
   python3 scripts/catalog-sync.py                # report drift, change nothing
   python3 scripts/catalog-sync.py --write        # rewrite the generated kinds
   ```

   The generated kinds — enchants and gems — are rewritten from Blizzard's own
   grouping. The **editorial** kinds — consumables and reagents — are only
   reported on, because `audience`, `stat` and `profession` are judgements the
   API cannot make and a rewrite would discard them silently. Read the drift
   report and edit those by hand.

4. **Deploy.** The patch's window is materialised on the next cycle, and it
   appears in the archive at once because the hierarchy is derived rather than
   stored. Nothing is activated: a patch is not a release state.

---

## B. A raid tier opened (a rollover)

`docs/market-analysis.md` §8: "New tiers introduce a new active catalogue; the
former active BoE tier stops collecting automatically and becomes a read-only
archive." So a rollover ships a **second catalogue for the same expansion**,
and activating it archives its predecessor in one transaction.

1. **Copy the active catalogue to a new id** in `catalogs.json` — `midnight-s2`
   becomes `midnight-s3` — keeping `expansion` **exactly the same string**.
   That is what makes the archive show one expansion rather than two: the
   hierarchy groups by the expansion's name, not by catalogue id.

2. **Carry the whole expansion's patch and tier list forward.** Not only the
   new ones. A tier's window ends at the next tier *its own catalogue*
   declares, so a catalogue that ships one tier and is then superseded has a
   tier window with no end — and it goes on absorbing the successor's prices
   for ever. That is a statistic that is *wrong* rather than absent, which is
   the kind nobody notices because it still renders.

   `/admin` refuses to let this go quiet: `Archive::problems` reports it above
   the release panel, naming both tiers and the catalogue to add the missing
   one to.

3. **Add the new tier**, with the day the raid opened — which is not the day
   the patch shipped:

   ```json
   {"id": "sunless-reach", "name": "The Sunless Reach",
    "patch": "12.2", "opened": "2026-11-10", "season": 3}
   ```

   `id` is a stable slug and goes in the URL. It never changes once shipped.

4. **Replace the bind-on-equip list** with the new raid's pieces, from
   `track.txt`, and resolve their bonus ids against a live realm:

   ```bash
   python3 scripts/catalog-sync.py --realm eu:1403 --write
   ```

   That writes `tracks` (the upgrade-track bonus ids) and `item_levels` (the
   rank bonus ids). Grouping is on the **track** bonus, never the rank — the
   market carries ranks no sync has resolved, and grouping on one of those
   would make a market named after nothing.

5. **Set `"status": "draft_ptr"`** on the new catalogue and bump
   `catalog_version`. The shipped status is only a seed for a database that
   has never seen the catalogue; after that the database is authoritative.

6. **Write reviewer notes** in the catalogue's `notes` array — what is guessed
   from a PTR build, what still needs checking. They appear at `/admin` and
   nowhere else: a PTR note is unconfirmed research and must not reach a
   public annotation.

7. **Deploy.** Nothing changes for a visitor. A `draft_ptr` catalogue is
   administrator-only: it is not in the expansion picker, not in the archive,
   and its candidate item ids resolve to 404 on the item and gear pages.

8. **Review it at `/admin`**, which is what the panel is for. It shows the
   patches, the tiers each opened, the item counts by kind, the notes, and
   anything `Catalog::problems` or `Archive::problems` found. Activation is
   refused outright while a catalogue's own data does not hold together — "an
   administrator explicitly activates it after reviewing it" is only worth
   anything if the review can say no.

9. **Press Activate** when the raid opens. One transaction: the new catalogue
   becomes active and the old one becomes archived. A partial unique index
   makes two active catalogues impossible even if the transaction is ever got
   wrong.

That is the whole operation. Afterwards:

- the new tier is collected, and only it — an archived catalogue's items are
  no longer fetched;
- last tier's prices are still at their own URLs, frozen: nothing new is
  observed for those markets, so nothing is staged for them, and `publish`
  leaves rows it did not recalculate exactly where they were;
- both tiers are in the archive under one expansion, each closed at the next;
- pressing Activate again is not a second rollover.

`crates/storage/tests/rollover.rs` runs all of that against a real database
and a real activation, and it is the test to read if any of it stops being
true.

---

## What is *not* part of this

- **A migration.** The read model's shape does not depend on how many
  catalogues there are.
- **A statistic.** `market::engine` decides what cheap means, once, for every
  tier there will ever be.
- **A route or a template.** The archive's four levels are the same four
  whatever is under them; the patch page fetches the consumables page's own
  patch fragment narrowed to one column, and the tier page draws the Gear
  page's own card macro over the Gear page's own stored roll-ups.

If a tier needs one of those, write down why in CLAUDE.md §16 before writing
it. §7's cost of getting this wrong is not aesthetic: it is that the reader
stops trusting that two pages showing the same word mean the same thing.
