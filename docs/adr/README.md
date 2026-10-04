# Architecture decision records

Significant design decisions in Cairn are recorded as ADRs. Each ADR is short and follows one fixed
template — **Context → Decision → Alternatives rejected → Consequences → References** — so the
decision can be read without reading the whole specification.

The normative source for every ADR is [`SPEC.md`](../../SPEC.md). An ADR never adds or overrides
anything the spec says; it restates one recorded decision with its rationale, its rejected
alternatives and pointers to the sections and tests that enforce it.

## Numbering convention

`ADR-00NN` ↔ decision `D-NN` from SPEC §2.1, zero-padded to four digits:

- `ADR-0001` ↔ `D-01` … `ADR-0018` ↔ `D-18` — one ADR per row of the decision table.
- Two extra ADRs cover decisions recorded outside the §2.1 table:
  `ADR-0019` records the checkpoint mechanism chosen in SPEC §9.8, and `ADR-0020` records the
  mid-stream SSE resume policy in SPEC §4.7 (referenced by REQ-PROV-009 / REQ-PROV-010).

## Status vocabulary

Each ADR carries a status line of the form `**Status:** Accepted (2026, Cairn 1.x)`.

- **Accepted** — decided and in force for the Cairn 1.x line, as recorded in SPEC §2.1 (or the
  spec section named in the ADR). All current ADRs are Accepted; a future change to an accepted
  decision would be made in `SPEC.md` first, then reflected here.

## Index

| ADR | Decision | Title | Status |
|-----|----------|-------|--------|
| [ADR-0001](ADR-0001.md) | D-01 | Language & runtime | Accepted |
| [ADR-0002](ADR-0002.md) | D-02 | Concurrency model | Accepted |
| [ADR-0003](ADR-0003.md) | D-03 | TUI framework | Accepted |
| [ADR-0004](ADR-0004.md) | D-04 | HTTP & streaming | Accepted |
| [ADR-0005](ADR-0005.md) | D-05 | Retry & backoff | Accepted |
| [ADR-0006](ADR-0006.md) | D-06 | Parsing | Accepted |
| [ADR-0007](ADR-0007.md) | D-07 | Search | Accepted |
| [ADR-0008](ADR-0008.md) | D-08 | Session storage | Accepted |
| [ADR-0009](ADR-0009.md) | D-09 | Index storage | Accepted |
| [ADR-0010](ADR-0010.md) | D-10 | Config format | Accepted |
| [ADR-0011](ADR-0011.md) | D-11 | Async runtime scope | Accepted |
| [ADR-0012](ADR-0012.md) | D-12 | Diff engine | Accepted |
| [ADR-0013](ADR-0013.md) | D-13 | Dependency policy | Accepted |
| [ADR-0014](ADR-0014.md) | D-14 | Licensing of Cairn itself | Accepted |
| [ADR-0015](ADR-0015.md) | D-15 | Error handling style | Accepted |
| [ADR-0016](ADR-0016.md) | D-16 | Serialization | Accepted |
| [ADR-0017](ADR-0017.md) | D-17 | Terminal color | Accepted |
| [ADR-0018](ADR-0018.md) | D-18 | Secrets at rest | Accepted |
| [ADR-0019](ADR-0019.md) | §9.8 | Shadow-ref checkpoint mechanism | Accepted |
| [ADR-0020](ADR-0020.md) | §4.7 | No mid-stream SSE resume | Accepted |
