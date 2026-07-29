# gnubg-engine-server

GNU Backgammon as a standalone analysis daemon: JSON-RPC 2.0 over
stdin/stdout, speaking the Backgammon Engine Protocol. The second
protocol implementation next to sage-engine-server — it exists both as a
real engine choice and to keep the protocol honest across engine
families (different ply conventions, different capability surfaces).

Licensed GPL-3.0-or-later, like gnubg. The process boundary is the
licensing boundary: hosts talk to the daemon over stdio and need not be
GPL.

Evaluations run through `libgnubgapi`, the C wrapper over gnubg's
evaluation core (the same native library GammonBase's cloud evaluator
P/Invokes), so desktop results are row-for-row identical to the cloud
gnubg engine family.

## What it serves

| Level | Kind | Notes |
|---|---|---|
| `0ply`–`4ply` | ply | gnubg ply convention: 0-ply = raw NN. Cube ply-parity rule: odd plies leave the opponent on roll at the leaves and bias cube equities, so `1ply`/`3ply` exclude `evaluateCube`; `4ply` is the deep-cube level (`evaluatePosition` + `evaluateCube` only — full-movelist 4-ply checker scoring is pathological) |
| `rollout` | rollout | position evaluation only (`methods: ["evaluatePosition"]`) — gnubgapi exposes no move/cube rollout; configurable per request (`levelOptions`: `trials`, `cubeful`, `varianceReduction`, `chequerPlies`, `cubePlies`, `seed`, `truncate`, `truncatePlies`) |

No progress or cancellation: the gnubgapi ABI cannot abort a running
evaluation (the spec allows completing normally).

gnubg's evaluator is one global neural net with static caches, so the
daemon holds exactly one engine context and declares `maxParallel: 1`.

## Layout

The daemon loads `libgnubgapi` at runtime. Default layout next to the
executable, every path overridable:

```
gnubg-engine-server[.exe]
libgnubgapi.so | libgnubgapi.dll | libgnubgapi.dylib
data/gnubg.weights
data/gnubg.wd
data/gnubg_os0.bd, data/gnubg_ts0.bd, data/met/
```

```
gnubg-engine-server [--gnubgapi-lib <path>] [--data-dir <dir>]
                    [--weights <path>] [--weights-binary <path>]
                    [--no-bearoff]
```

`GNUBGAPI_LIB` overrides the library path when the flag is absent.
Missing files fail fast at startup.

## Testing

- `cargo test` — unit tests (level resolution, notation, cube-decision
  collapse); no engine library required.
- `tests/e2e/run_e2e.py` — spawns the built daemon and drives it over
  stdio. `tests/e2e/container_e2e.sh` runs it in a Linux container
  against the prebuilt `libgnubgapi.so` from the gnubgapi repository.
