<h1 align="center">Tordo</h1>
<p align="center"><img src="static/favicon.svg" width="96" alt="Tordo"></p>
<p align="center"><a href="README.md">English</a> · <strong>Español</strong></p>

Crea tu propio catálogo navegable de películas y series enlazadas con
[Rucio](https://github.com/ogarcia/rucio). Es un pequeño generador de sitios
estáticos: describes cada título en un fichero TOML, obtiene los metadatos y las
carátulas de [TMDB](https://www.themoviedb.org/) y genera un conjunto local de
páginas HTML que abres en tu navegador — un índice ordenado de tu propio
catálogo, para tu uso.

## Cómo funciona

Cada entrada es un fichero TOML en `content/` que describe su `type`/`id` de
TMDB y una lista plana de `[[releases]]` (cada uno un conjunto de enlaces Rucio
con metadatos opcionales de fuente/resolución/hdr/códec/idioma/… que se
normalizan en insignias, y su propia `date`). La posición de una entrada en la
portada es la más reciente de las fechas de sus releases, así que añadir enlaces
la vuelve a sacar como actualizada. Mira [`examples/`](examples/) para una
entrada documentada al completo; cuando `content/` no tiene ninguna entrada se
renderizan esos ejemplos, de modo que un clon recién hecho ya muestra una demo.
`content/` se recorre de forma **recursiva**, así que su organización es libre —
`tordo import` coloca las entradas en `content/tv/` y `content/movies/`, pero
vale plano o cualquier otra disposición.

Al construir, el generador obtiene los datos de TMDB (cacheados en `cache/`) y
renderiza la portada más una página de detalle por entrada en `dist/`. El
resultado son ficheros estáticos: abre `dist/index.html` en tu navegador. La
búsqueda y la ordenación se ejecutan en el navegador, así que nada necesita un
servidor.

## Construir

`tordo` tiene tres comandos: `generate` (construir el sitio, el que se usa por
defecto si no indicas ninguno), `import` y `merge` (ver más abajo). `cargo run`
equivale a `cargo run -- generate`:

```sh
export TMDB_READ_TOKEN=...   # token de lectura v4 de TMDB (o TMDB_API_KEY)
cargo run                    # == tordo generate; salida en dist/
```

Las entradas cuyo payload de TMDB ya está cacheado se construyen sin clave ni
red. El token es gratis: crea una cuenta en TMDB y copia tu **Read Access Token**
(v4) de la API en [Ajustes → API](https://www.themoviedb.org/settings/api) — o usa
la **API Key** v3 como `TMDB_API_KEY`.

Copia antes `config.example.toml` a `config.toml` (ver Configuración). Tu
catálogo — su `config.toml`, `content/` y `cache/` — puede vivir en su propio
directorio; apunta el build hacia él con `CATALOG_DIR` (un catálogo también
puede traer su propia imagen de vista previa, vía `og_image`):

```sh
CATALOG_DIR=/ruta/al/catalogo cargo run
```

Sin `CATALOG_DIR` el build lee este directorio (y recurre a `config.example.toml`
cuando no hay `config.toml`, renderizando la demo). Tu `config.toml`, `content/`
y `cache/` describen exactamente qué hay en tu catálogo — ten cuidado de no
compartir su contenido.

## Importar desde una base de datos Rucio

`tordo import` lee la tabla `shared_files` de una base de datos Rucio, agrupa los
ficheros de vídeo compartidos en obras, resuelve cada una contra TMDB y escribe
las entradas `content/<tipo>-<id>.toml` correspondientes — deduplicando contra lo
que ya existe:

```sh
export TMDB_READ_TOKEN=...
cargo run -- import          # pregunta por la base de datos y el directorio de contenido
```

Sin `--db`, se usa la propia base de datos del daemon de Rucio. Flags útiles:
`--content DIR`, `--plan` (vista previa sin red), `--dry-run` (consulta TMDB, no
escribe), `--auto` (salta las coincidencias dudosas), `--limit N`; `tordo import
--help` los lista todos.

## Fusionar contenido

`tordo merge <BASE> <OTRO>` fusiona un segundo directorio de contenido sobre el
primero, cotejando por hash de enlace: cada entrada de `OTRO` se añade a `BASE`, y
para las entradas que ya existen ahí solo se incorporan los enlaces **nuevos** —
no se sobrescribe nada. Usa `--dry-run` para previsualizar los cambios.

```sh
cargo run -- merge content ../otro-catalogo/content
```

## Contenedor

Una imagen ya construida empaqueta `tordo` y sus recursos, así que puedes ejecutar
cualquiera de sus comandos sin tener Rust instalado. Monta el catálogo en
`/catalog`:

```sh
docker run --rm -v "$PWD:/catalog" ghcr.io/ogarcia/tordo          # generate
docker run --rm -v "$PWD:/catalog" ghcr.io/ogarcia/tordo import   # import
```

`generate` lee `/catalog/config.toml`, `/catalog/content/` y `/catalog/cache/` y
escribe `/catalog/dist/`. Añade `-e TMDB_READ_TOKEN=...` cuando alguna entrada
aún no esté cacheada.

## Caché de TMDB

Para ser buen ciudadano con la API, los payloads se cachean en `cache/` y se
reutilizan:

- **Películas y series finalizadas** (`Ended`/`Canceled`) se obtienen **una vez**
  y no se refrescan automáticamente.
- **Series en emisión** se refrescan cuando su caché supera los
  `tmdb_refresh_days` (30 por defecto). El momento de la obtención se guarda
  dentro del JSON cacheado (`_fetched_at`), así sobrevive a los checkouts de git
  (los mtime de fichero no).

Un refresco fallido (sin red/credencial) recurre a la copia cacheada en vez de
romper el build. Dos flags fuerzan un refresco:

```sh
cargo run -- generate --refresh-live   # re-obtener solo series en emisión
cargo run -- generate --refresh-all    # re-obtener todo
```

## Configuración

Copia `config.example.toml` a `config.toml` y pon el `title` del catálogo y el
`lang` de interfaz/contenido (los textos de interfaz están en
`i18n/<lang>.toml`). La clave de API nunca se guarda en el config. Todo lo demás
del fichero es opcional.

## Variables de entorno

| Variable | Comando | Para qué |
|---|---|---|
| `TMDB_READ_TOKEN` | generate, import | token de lectura v4 de TMDB (preferido) |
| `TMDB_API_KEY` | generate, import | clave de API v3 de TMDB (alternativa) |
| `CATALOG_DIR` | generate | directorio con `config.toml`, `content/`, `cache/` (por defecto: directorio actual) |
| `CATALOG_OUT` | generate | directorio de salida del sitio construido (por defecto: `dist`) |
| `RUCIOD_DB_PATH` | import | ruta explícita a la base de datos Rucio |
| `RUCIOD_BASE_DIR` | import | directorio base con `rucio.db` (si `RUCIOD_DB_PATH` no está) |
| `NO_COLOR` / `FORCE_COLOR` | import | desactivar / forzar el color en terminal |

La credencial de TMDB también se puede pasar con `--tmdb-token`/`--tmdb-key`, y la
base de datos con `--db`. Cuando no se indica ninguna ruta de base de datos (ni
`--db` ni estas variables), `import` busca la base de datos por defecto del daemon
de Rucio. En el contenedor se indican con `-e NOMBRE=valor`.
