# Documentation

| Set | Audience | Source | Contents |
|---|---|---|---|
| **Internal** | Contributors | [`spec/`](../spec/) (book config in [`docs/internal/`](internal/)) | Design specs, invariants, decisions and milestones, and the internal API reference |
| **Public** | People who use, install and operate Hyperstor, and developers who want to know how it works | [`docs/public/`](public/), landing page in [`docs/site/`](site/) | Quick start, user guide, administrator guide, developer guide, reference, status |

- **Internal** pages describe *why the code is shaped the way it is*.
  They stay in `spec/` because code cites them as `spec/<file>.md`;
  don't move or rename them once code refers to them.
- **Public** pages describe *what Hyperstor does and how to use it*, must
  not depend on internal pages, and must not contain security-sensitive
  details (internal hostnames, credentials, attack-surface analysis).
  When a design ships, write its user-facing part there and link from the
  spec to it. The developer guide explains the design for outside
  readers: keep it self-contained (no links into `spec/`, no decision
  numbers), and update its figures when `spec/02-benchmarks.md` changes.
- **Generated or checked against the code:**
  - `docs/public/src/status.md` is generated from `spec/tracking.toml`
    (`scripts/tracking.py status > docs/public/src/status.md`); CI fails
    if it is stale.
  - The usage line and option table of each binary's page in
    `docs/public/src/reference/` are checked against the binary
    (`scripts/cli-docs-check.py`). Change the page with the options.
  - The client API reference is rustdoc of `hyperstor-client` and
    `hyperstor-reactor`.

## Building

Both books use [mdBook](https://rust-lang.github.io/mdBook/) 0.5
(`cargo install mdbook --locked`):

```sh
mdbook build docs/public      # -> target/docs/public
mdbook build docs/internal    # -> target/docs/internal
mdbook serve docs/public      # live preview on http://localhost:3000

# Public client API reference
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p hyperstor-client -p hyperstor-reactor

# Internal API reference: every crate, private items included
RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links" \
  cargo doc --workspace --no-deps --document-private-items   # -> target/doc
```

CI builds both on every pull request
([`.github/workflows/docs.yml`](../.github/workflows/docs.yml)), checks
the site's links (`scripts/site-link-check.py`) and keeps the output as
workflow artifacts. On every push to `main` it publishes the public site
to the `gh-pages` branch: the landing page at the root, the public book
under `docs/` and the client API reference under `api/`
(<https://cmkl-apex.github.io/hyperstor/>, once GitHub Pages serves that
branch). The internal set is never published there; it must only ever go
to an access-controlled host.
