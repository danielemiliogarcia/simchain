<!--
Thanks for contributing. Keep the pull request focused on one concern and
explain both what changed and why — the "why" is what reviewers cannot
reconstruct from the diff. See CONTRIBUTING.md.
-->

## What changed

<!-- The change itself, in a sentence or two. -->

## Why

<!-- The problem, use case, or bug this addresses. -->

Closes #

## Checks run

<!-- Delete lines that do not apply to this change. -->

```bash
cargo fa
cargo ba && cargo ca && cargo fac && cargo tt
```

- [ ] `cargo ba` (build all targets)
- [ ] `cargo ca` (clippy, `-D warnings`)
- [ ] `cargo fac` (format check)
- [ ] `cargo tt` (tests, single-threaded)
- [ ] `Cargo.lock` committed if dependencies changed

If Compose, Dockerfiles, or shell scripts changed:

- [ ] `docker compose config --quiet`
- [ ] `./scripts/check-compose-security.sh`
- [ ] `./scripts/check-docker-images.sh`
- [ ] Exercised the affected profile or script against a live stack

## Manual scenarios exercised

<!-- Which profile, commands, or scenario files you ran, and what you observed.
     Write "none — docs only" if that is the case. -->

## Project intent

- [ ] No relay, mempool, or capacity policy added that diverges from Bitcoin Core's
      mainnet defaults
- [ ] Control plane gained no Docker socket, Docker CLI, repository bind mount, or
      process-lifecycle executor
- [ ] Any helper needed by a second tool lives in `crates/simchain-common`
- [ ] Docs, `.env.example` / `.env.full.example`, and tests updated for behavior or
      settings changes
- [ ] Shipped items removed from `docs/NICE-TO-HAVE.md` (not marked done)
