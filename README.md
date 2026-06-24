# DOB-EVO

DOB-EVO is a production-oriented CellScript profile for evolving Digital Object state on CKB without mutating the original Spore DOB Cell.

The core idea is simple:

```text
immutable Spore DOB + live DobEvolutionStateV1 + decoder = current render state
```

The base DOB remains stable. Evolution happens through a typed successor state Cell with explicit actions, fixtures, schemas, and proof evidence.

## Why You Might Use This

Use DOB-EVO if you are building or reviewing:

- dynamic DOB art or metadata that should keep a stable immutable origin;
- CKB state lines where every evolution step needs a typed proof surface;
- CellScript package registry flows with production-style verification gates;
- audit material for DOB evolution, finalisation, replay prevention, and owner-lock preservation.

This repository is standalone. It is also consumed by the main CellScript compiler repository as a submodule at:

```text
proposals/evolving-dob/evolving-dob-profile-v1
```

## What Is Included

| Path | Purpose |
| --- | --- |
| `src/evolving_dob_type.cell` | The CellScript source for initialise, evolve, and finalise actions. |
| `schemas/` | State, intent, and event layout notes. |
| `fixtures/` | Positive and negative scenario labels for builders and VM harnesses. |
| `proofs/` | Invariant matrix and proof-plan material. |
| `docs/` | Profile, security, production-readiness, and registry-pressure notes. |
| `scripts/evolving_dob_registry_pressure.py` | Local package and registry pressure check. |
| `scripts/evolving_dob_devnet_workflow.py` | Local CKB devnet workflow for deployment and live registry evidence. |

## Quick Start

Install or build `cellc`, then run the local package checks:

```bash
cellc build --release --target riscv64-elf --target-profile ckb
cellc check --target-profile ckb --primitive-strict 0.16
cellc package verify --json
cellc publish --dry-run
python3 scripts/evolving_dob_registry_pressure.py
```

These commands are intended to prove that the source package is internally coherent before you wire it into a registry or deployment process.

## Local Devnet Evidence

If you have a CKB checkout or `CKB_BIN` available, run:

```bash
python3 scripts/evolving_dob_devnet_workflow.py --pretty
```

That workflow starts a local integration node, deploys the built type script as a live code Cell, writes deployment metadata, verifies the registry identity including `--live`, emits action build plans, generates the TypeScript builder, and runs the generated builder tests.

This is local-node evidence. It is not a claim that the package has already been deployed to public Aggron or mainnet infrastructure.

## Production Boundary

DOB-EVO/1 intentionally does not support legacy evolving-DOB encodings. It is scoped to the `DobEvolutionStateV1` model and the invariants documented under `docs/` and `proofs/`.

Start with:

- `docs/PROFILE.md`
- `docs/PRODUCTION_READINESS.md`
- `docs/SECURITY.md`
- `proofs/invariant_matrix.json`
