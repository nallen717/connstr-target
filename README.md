# ctarget

Connection strings look simple until you have to answer "what does this
actually connect to." A missing port silently falls back to whatever the
driver defaults to. A comma-separated host list is tried in order, not load
balanced, and drivers disagree about how many hosts they'll accept. A stray
`sslmode=disable` in a query string turns off TLS with no other visual cue.
Pasting the string into a bug report leaks a password unless you remember to
redact it by hand.

`ctarget` takes one connection string and prints exactly what it resolves
to: the ordered list of hosts with ports filled in, whether TLS is implied,
the database name, and whether a password is present (without printing it).

It does not connect to anything. It only parses.

## Usage

```
$ ctarget 'postgres://app:secret@db1:5432,db2:5432/orders?sslmode=require'
scheme:   postgres
auth:     app:*** (password present in string)
hosts:    2 target(s), tried in order
  1. db1:5432
  2. db2:5432
database: orders
tls:      likely on, sslmode=require
params:
  sslmode = require
```

A string with no explicit port shows where the default came from:

```
$ ctarget 'redis://cache.internal/0'
scheme:   redis
auth:     none
hosts:    1 target(s), tried in order
  1. cache.internal:6379 (default for scheme)
database: 0
tls:      no indication in string
```

You can also pipe it in, which is handy for pulling a string out of an env
file or a secrets manager without it showing up in your shell history:

```
$ echo "$DATABASE_URL" | ctarget
```

libpq's `key=value` style is recognized too, since plenty of tools (psql,
`PGSERVICE` entries, `libpq.conf`) use it instead of a URI:

```
$ ctarget "host=db1,db2 port=5432 dbname=orders user=app sslmode=require"
scheme:   postgres (libpq key=value)
auth:     app (no password)
hosts:    2 target(s), tried in order
  1. db1:5432
  2. db2:5432
database: orders
tls:      likely on, sslmode=require
params:
  sslmode = require
```

A single `port=` applies to every host in the `host=` list; if you give more
than one port, the counts have to match. Leaving `host=` out entirely is
recognized as libpq's local unix-socket fallback, but the actual socket path
isn't resolved (see below).

## Supported schemes

Recognizes default ports and TLS conventions for `postgres`/`postgresql`,
`mysql`, `mongodb`, `mongodb+srv`, `redis`, `rediss`, `amqp`, `amqps`,
`http`, and `https`. Anything else still parses, it just won't have a
default port to fall back on.

## What it does not do

- No DNS resolution and no SRV record lookups (`mongodb+srv` is recognized
  by scheme but the real host list behind an SRV record isn't fetched).
- No resolution of libpq's unix-socket default when `host=` is omitted -
  it's reported as such, not given a real path.
- No connecting, no credential validation, no network access at all.

## Build

Standard library only, nothing to fetch.

```
cargo build --release
```

## License

MIT, see LICENSE.
