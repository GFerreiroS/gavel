# Translations

The interface is translated with plain [gettext PO files][po]. Nothing here is
specific to a platform: Weblate, Crowdin and Transifex all read and write this
layout unchanged.

```
locales/
├── messages.pot   generated from the source; never edited by hand
├── es.po          Spanish
└── <lang>.po      one file per language
```

Catalogues are per **language**, not per locale: `es_ES` and `es_MX` share
`es.po`, because the interface wording does not differ between them. Item
names, effects and tooltips are a separate matter entirely -- those come from
Battle.net in all twelve locales and need no translator.

## The source string is the key

```po
msgid "Raid consumables"
msgstr "Consumibles de banda"
```

There are no keys like `market.title`. The msgid *is* the English text, so an
untranslated or missing string renders as English rather than as a broken
identifier. An empty `msgstr` is the same as no entry at all.

## `{}` placeholders

Some strings carry a value:

```po
msgid "every realm in {} sees the same market."
msgstr "todos los reinos de {} ven el mismo mercado."
```

Each `{}` is filled in order with a value the page supplies -- a region, a node
name, a count. A translation must keep the same number of `{}`, but is free to
move them: that is the whole reason the value travels inside the string instead
of being glued on around it.

Identifiers are never inside the translatable part. `"{} joined"` translates;
`node-03` does not.

## Adding a language

The file name is the two-letter language, and it must be one the item data
also comes in -- the twelve in `app_core::locale`. A `.po` named after anything
else compiles but can never be selected, and a unit test fails rather than
letting that sit there unnoticed.

```sh
cp locales/messages.pot locales/fr.po     # or let the platform create it
# set  "Language: fr\n"  in the header, then fill in the msgstr lines
cargo build                               # build.rs picks up any new .po
```

That is the whole procedure: no code change, no registration, no list to add
the language to. Restart the server and it is in the top-bar menu.

A partial catalogue is fine. Every translated string is used and the rest fall
back to English, string by string. Until a language passes
`INTERFACE_THRESHOLD_PERCENT` (80%) the menu keeps labelling it *item text
only*, so nobody is promised a translation that is mostly English.

`python3 scripts/i18n-extract.py` prints the coverage of every catalogue:

```
locales/messages.pot: 250 strings
es.po: 250/250 translated (100%)
fr.po: 3/250 translated (1%)
```

## Adding or changing a string

1. Edit the template: `{{ "New heading"|t }}`.
2. Regenerate the template: `python3 scripts/i18n-extract.py`.
3. Let the translation platform merge `messages.pot` into each `.po`.

The extractor reads two sources: the `|t` calls in `crates/app-web/templates`,
and `EXTERNAL_STRINGS` in `crates/app-web/src/i18n.rs` -- the labels that reach
a page from another crate (a role name, a job state), which a template only
ever sees as `{{ node.status|t }}`. A unit test keeps that list exhaustive, so
adding a `Role` variant without listing it fails the build rather than quietly
rendering in English.

## How it reaches the binary

`crates/app-web/build.rs` compiles every `.po` into a sorted static table at
build time. There is no runtime PO parser, no catalogue files to ship and no
allocation per lookup. Deployment stays a single self-contained binary.

A rebuild is required after changing a catalogue; `cargo` handles that through
`rerun-if-changed`.

[po]: https://www.gnu.org/software/gettext/manual/html_node/PO-Files.html
