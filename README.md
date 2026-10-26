**English** · [Español](README.es.md)

# Tordo

Build your own browsable catalog of films and series linked with
[Rucio](https://github.com/ogarcia/rucio). It is a small static-site generator:
you describe each title in a TOML file, it pulls the metadata and cover art from
[TMDB](https://www.themoviedb.org/) and renders a local set of HTML pages you
open in your browser — a tidy index of your own catalog, for your own use.

## How it works

Each entry is a TOML file in `content/` describing its TMDB `type`/`id` and a
flat list of `[[releases]]` (each a set of Rucio links with optional
source/resolution/hdr/codec/language/… metadata that is normalized into display
badges, and its own `date`). The entry's position on the home page is the most
recent of its releases' dates, so adding links resurfaces it as updated. See
[`examples/`](examples/) for a fully documented entry; when `content/` has no
entry those examples are rendered instead, so a fresh checkout still shows a
demo. `content/` is scanned **recursively**, so its layout is free — the import
tool files entries under `content/tv/` and `content/movies/`, but flat or any
other arrangement works.

At build time the generator fetches TMDB data (cached under `cache/`) and renders
the home page plus one detail page per entry into `dist/`. The result is plain
static files: open `dist/index.html` in your browser. Search and sorting run
client-side, so nothing needs a server.

## Build

```sh
export TMDB_READ_TOKEN=...   # TMDB v4 read access token (or TMDB_API_KEY)
cargo run
# output in dist/
```

Entries whose TMDB payload is already cached build without a key or network.

Copy `config.example.toml` to `config.toml` first (see Configuration). Your
catalog — its `config.toml`, `content/` and `cache/` — can live in its own
directory; point the build at it with `CATALOG_DIR` (a catalog can ship its own
link-preview image too, via `og_image`):

```sh
CATALOG_DIR=/path/to/catalog cargo run
```

With no `CATALOG_DIR` the build reads this directory (and falls back to
`config.example.toml` when there is no `config.toml`, rendering the demo). Your
`config.toml`, `content/` and `cache/` describe exactly what is in your catalog —
take care not to share their contents.

## TMDB caching

To stay a good API citizen, payloads are cached under `cache/` and reused:

- **Films and finished series** (`Ended`/`Canceled`) are fetched **once** and
  never refreshed automatically.
- **Ongoing series** are refreshed once their cache is older than
  `tmdb_refresh_days` (default 30). The fetch time is stored inside the cached
  JSON (`_fetched_at`), so it survives git checkouts (file mtimes don't).

A refresh that fails (no network/credential) falls back to the cached copy
instead of breaking the build. Two flags force a refresh:

```sh
cargo run -- --refresh-live   # re-fetch ongoing series only
cargo run -- --refresh-all    # re-fetch everything
```

## Configuration

Copy `config.example.toml` to `config.toml` and set the catalog `title` and the
UI/content `lang` (UI strings live in `i18n/<lang>.toml`). The API key is never
stored in config. Everything else in the file is optional.
