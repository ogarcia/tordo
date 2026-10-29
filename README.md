<h1 align="center">Tordo</h1>
<p align="center"><img src="static/favicon.svg" width="96" alt="Tordo"></p>
<p align="center"><strong>English</strong> · <a href="README.es.md">Español</a></p>

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
demo. `content/` is scanned **recursively**, so its layout is free — `tordo
import` files entries under `content/tv/` and `content/movies/`, but flat or any
other arrangement works.

At build time the generator fetches TMDB data (cached under `cache/`) and renders
the home page plus one detail page per entry into `dist/`. The result is plain
static files: open `dist/index.html` in your browser. Search and sorting run
client-side, so nothing needs a server.

## Build

`tordo` has three commands: `generate` (build the site, the default when none is
given), `import`, and `merge` (see below). `cargo run` is `cargo run -- generate`:

```sh
export TMDB_READ_TOKEN=...   # TMDB v4 read access token (or TMDB_API_KEY)
cargo run                    # == tordo generate; output in dist/
```

Entries whose TMDB payload is already cached build without a key or network. The
token is free: create a TMDB account and copy your API **Read Access Token** (v4)
from [Settings → API](https://www.themoviedb.org/settings/api) — or use the v3
**API Key** as `TMDB_API_KEY`.

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

## Importing from a Rucio database

`tordo import` reads the `shared_files` table of a Rucio database, groups the
shared video files into works, resolves each against TMDB and writes the matching
`content/<type>-<id>.toml` entries — deduplicating against what is already there:

```sh
export TMDB_READ_TOKEN=...
cargo run -- import          # asks for the database and content directory
```

With no `--db`, the Rucio daemon's own database is used. Useful flags:
`--content DIR`, `--plan` (offline preview), `--dry-run` (query TMDB, write
nothing), `--auto` (skip uncertain matches), `--limit N`; `tordo import --help`
lists them all.

Without a TMDB credential you can still extract the links offline with
`--export links.json`: it groups the database into works and writes their links
and release details to a file, no TMDB. Someone with a credential then finishes
the import from that file with `--from-works links.json` instead of a database.
Links already present in the target catalog are skipped, so re-importing is safe.

## Merging content

`tordo merge <BASE> <OTHER>` folds a second content directory into the first,
matching by link hash: every entry in `OTHER` is added to `BASE`, and for entries
already present there only the **new** links are merged in — nothing is
overwritten. Pass `--dry-run` to preview the changes.

```sh
cargo run -- merge content ../other-catalog/content
```

## Container

A prebuilt image bundles `tordo` and its assets, so you can run any of its
commands without a Rust toolchain. Mount the catalog at `/catalog`:

```sh
docker run --rm -v "$PWD:/catalog" ghcr.io/ogarcia/tordo          # generate
docker run --rm -v "$PWD:/catalog" ghcr.io/ogarcia/tordo import   # import
```

`generate` reads `/catalog/config.toml`, `/catalog/content/` and `/catalog/cache/`
and writes `/catalog/dist/`. Add `-e TMDB_READ_TOKEN=...` when some entries are
not yet cached.

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
cargo run -- generate --refresh-live   # re-fetch ongoing series only
cargo run -- generate --refresh-all    # re-fetch everything
```

## Configuration

Copy `config.example.toml` to `config.toml` and set the catalog `title` and the
UI/content `lang` (UI strings live in `i18n/<lang>.toml`). The API key is never
stored in config. Everything else in the file is optional.

## Environment variables

| Variable | Command | Purpose |
|---|---|---|
| `TMDB_READ_TOKEN` | generate, import | TMDB v4 read token (preferred) |
| `TMDB_API_KEY` | generate, import | TMDB v3 API key (alternative) |
| `CATALOG_DIR` | generate | directory holding `config.toml`, `content/`, `cache/` (default: current dir) |
| `CATALOG_OUT` | generate | output directory for the built site (default: `dist`) |
| `RUCIOD_DB_PATH` | import | explicit path to the Rucio database |
| `RUCIOD_BASE_DIR` | import | base dir holding `rucio.db` (if `RUCIOD_DB_PATH` is unset) |
| `NO_COLOR` / `FORCE_COLOR` | import | disable / force coloured terminal output |

The TMDB credential can also be passed with `--tmdb-token`/`--tmdb-key`, and the
database with `--db`. When no database path is given (neither `--db` nor these
variables), `import` looks for the Rucio daemon's default database. In the
container these are set with `-e NAME=value`.
