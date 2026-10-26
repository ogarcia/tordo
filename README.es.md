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

```sh
export TMDB_READ_TOKEN=...   # token de lectura v4 de TMDB (o TMDB_API_KEY)
cargo run
# salida en dist/
```

Las entradas cuyo payload de TMDB ya está cacheado se construyen sin clave ni
red.

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

## Contenedor

Una imagen ya construida empaqueta el generador y sus recursos, así que puedes
construir un catálogo sin tener Rust instalado. Monta el catálogo en `/catalog`:

```sh
docker run --rm -v "$PWD:/catalog" ghcr.io/ogarcia/tordo
# el sitio construido se escribe en ./dist
```

Lee `/catalog/config.toml`, `/catalog/content/` y `/catalog/cache/` y escribe
`/catalog/dist/`. Añade `-e TMDB_READ_TOKEN=...` cuando alguna entrada aún no
esté cacheada.

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
