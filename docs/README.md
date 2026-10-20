# Documentation

| Set | Audience | Source | Contents |
|---|---|---|---|
| **Internal** | Contributors | [`spec/`](../spec/) (book config in [`docs/internal/`](internal/)) | Design specs, invariants, decisions and milestones |
| **Public** | People who install, use and operate Hyperstor | `docs/public/` (to come with the first release) | Quick start, operator guide, API reference |

- **Internal** pages describe *why the code is shaped the way it is*.
  They stay in `spec/` because code cites them as `spec/<file>.md`;
  don't move or rename them once code refers to them.
- **Public** pages, when they exist, describe *what Hyperstor does and how
  to use it*, must not depend on internal pages, and must not contain
  security-sensitive details. When a design ships, write its user-facing
  part there and link from the spec to it.

Build the internal book with [mdBook](https://rust-lang.github.io/mdBook/)
(`cargo install mdbook --locked`):

```sh
mdbook build docs/internal    # -> target/docs/internal
mdbook serve docs/internal    # live preview on http://localhost:3000
```

The internal set must only ever be published to an access-controlled
host.
