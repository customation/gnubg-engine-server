# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Customation AS
"""End-to-end test: drive gnubg-engine-server over stdio like a host would.

Python is the CI referee only — it never ships. Structural and sanity
assertions; exact cross-checks against the cloud GnuBgApiEvaluator are
the parity harness's job.

Usage: run_e2e.py <server-binary> <gnubgapi-lib> <data-dir>
"""

import json
import subprocess
import sys
import threading

STARTING_POSITION_ID = "4HPwATDgc/ABMA"
MONEY_MATCH_ID = "cAgAAAAAAAAA"


class Client:
    def __init__(self, process):
        self.process = process
        self.next_id = 0
        self.responses = {}
        self.condition = threading.Condition()
        self.reader = threading.Thread(target=self._read_loop, daemon=True)
        self.reader.start()

    def _read_loop(self):
        stream = self.process.stdout
        while True:
            headers = {}
            line = stream.readline()
            if not line:
                return
            while line and line.strip():
                name, _, value = line.decode("ascii").partition(":")
                headers[name.strip().lower()] = value.strip()
                line = stream.readline()
            body = stream.read(int(headers["content-length"]))
            message = json.loads(body)
            with self.condition:
                if "id" in message and ("result" in message or "error" in message):
                    self.responses[message["id"]] = message
                self.condition.notify_all()

    def request(self, method, params=None, timeout=600):
        self.next_id += 1
        request_id = self.next_id
        payload = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            payload["params"] = params
        body = json.dumps(payload).encode("utf-8")
        self.process.stdin.write(b"Content-Length: %d\r\n\r\n%b" % (len(body), body))
        self.process.stdin.flush()
        with self.condition:
            if not self.condition.wait_for(
                lambda: request_id in self.responses, timeout=timeout
            ):
                raise TimeoutError(f"no response to {method} (id {request_id})")
            return self.responses.pop(request_id)


def expect(condition, message):
    if not condition:
        raise AssertionError(message)


def result_of(response):
    expect("error" not in response, f"unexpected error: {response.get('error')}")
    return response["result"]


def main():
    server, gnubgapi_lib, data_dir = sys.argv[1:4]
    process = subprocess.Popen(
        [server, "--gnubgapi-lib", gnubgapi_lib, "--data-dir", data_dir],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=sys.stderr.buffer,
    )
    client = Client(process)
    failures = []

    def check(name, fn):
        try:
            fn()
            print(f"PASS {name}")
        except Exception as ex:  # noqa: BLE001 — a referee reports, then re-raises at exit
            print(f"FAIL {name}: {ex}")
            failures.append(name)

    def base_params(level, **extra):
        return {
            "positionId": STARTING_POSITION_ID,
            "matchId": MONEY_MATCH_ID,
            "level": level,
            **extra,
        }

    def describe():
        result = result_of(client.request("describe"))
        expect(result["engine"]["family"] == "gnubg", "family")
        expect(result["conventions"]["plyCounting"] == "gnubg", "ply convention")
        level_ids = [level["id"] for level in result["levels"]]
        expect(
            level_ids == ["0ply", "1ply", "2ply", "3ply", "4ply", "rollout"],
            f"levels {level_ids}",
        )
        # No level declines anything: an absent `methods` means all four
        # (spec §7). The ply-parity rule for the cube is real but is the
        # host's to apply when CHOOSING a level, not the engine's to refuse
        # when asked; and the rollout binds position, cube and moves alike.
        for level in result["levels"]:
            expect(
                level.get("methods") is None,
                f"{level['id']} should decline nothing, got {level.get('methods')}",
            )
        rollout = result["levels"][-1]
        expect(rollout["configurable"] is True, "rollout configurable")

    check("describe", describe)

    def evaluate_position():
        result = result_of(client.request("evaluatePosition", base_params("0ply")))
        expect(0.50 < result["WinProb"] < 0.58, f"start win prob {result['WinProb']}")
        expect(-0.2 < result["Equity"] < 0.2, f"start equity {result['Equity']}")
        expect(-0.6 < result["CubefulEquity"] < 0.6, f"start cubeful {result['CubefulEquity']}")

    check("evaluatePosition 0ply", evaluate_position)

    def evaluate_moves():
        result = result_of(
            client.request("evaluateMoves", base_params("0ply", die1=3, die2=1))
        )
        expect(result["Die1"] == 1 and result["Die2"] == 3, "dice canonicalized")
        alternatives = result["Alternatives"]
        expect(len(alternatives) > 5, f"got {len(alternatives)} alternatives")
        best = alternatives[0]
        expect(best["Rank"] == 1, "rank 1-based")
        expect(best["MoveNotation"] == "8/5 6/5", f"best {best['MoveNotation']}")
        expect(best["Plies"] == 0, f"gnubg-convention plies stamp {best['Plies']}")
        expect(best["ErrorVsBest"] == 0.0, "best ErrorVsBest")
        expect(
            best["GnubgPositionId"] == "4HPwATDgc/ABMA==",
            f"storage id {best['GnubgPositionId']}",
        )
        expect(alternatives[1]["ErrorVsBest"] > 0, "second worse than best")

    check("evaluateMoves 0ply 3-1", evaluate_moves)

    def analyze_move():
        # Hop order deliberately differs from gnubg's own formatting;
        # matching is order-insensitive over the hop multiset.
        result = result_of(
            client.request(
                "analyzeMove",
                base_params("0ply", die1=3, die2=1, move="24/23 13/10"),
            )
        )
        expect(result["Best"]["MoveNotation"] == "8/5 6/5", "best")
        played_hops = sorted(result["Played"]["MoveNotation"].split())
        expect(played_hops == ["13/10", "24/23"], f"played matched: {played_hops}")
        expect(result["Played"]["ErrorVsBest"] > 0, "played error positive")

    check("analyzeMove notation match", analyze_move)

    def evaluate_cube():
        result = result_of(client.request("evaluateCube", base_params("2ply")))
        expect(result["RecommendedAction"] == 0, f"start is no-double: {result}")
        expect(result["TooGoodToDouble"] is False, "not too good")
        expect(result["DropEquity"] == 1.0, "DP normalized to +1")
        expect(0.4 < result["OurWinProb"] < 0.6, "no-double dist present")
        expect(result["OppWinProb"] is not None, "take dist present (gnubg family)")

    check("evaluateCube 2ply", evaluate_cube)

    def rollout_small():
        result = result_of(
            client.request(
                "evaluatePosition",
                base_params("rollout", levelOptions={"trials": 36}),
                timeout=900,
            )
        )
        expect(0.4 < result["WinProb"] < 0.65, f"rollout win {result['WinProb']}")

    check("rollout evaluatePosition", rollout_small)

    def rollout_cube_small():
        result = result_of(
            client.request(
                "evaluateCube",
                base_params("rollout", levelOptions={"trials": 36}),
                timeout=900,
            )
        )
        # The action comes from FindCubeDecision over the ROLLED-OUT
        # outputs, so a rolled-out cube is a complete decision, not just
        # two rows of probabilities.
        expect("RecommendedAction" in result, f"cube rollout decision: {result}")
        expect(0.4 < result["OurWinProb"] < 0.65, f"cube rollout win {result['OurWinProb']}")

    check("rollout evaluateCube", rollout_cube_small)

    def rollout_moves_small():
        # Small on purpose, and the timing is the point: these options must
        # actually reach gnubg. RolloutGeneral takes its context from the
        # GLOBAL rcRollout for a fresh rollout, so a shim that sets only the
        # per-move esMove.rc silently runs 1296 trials instead of 36 —
        # indistinguishable from "rollouts are slow" unless something asks
        # for a small one and expects it to finish.
        result = result_of(
            client.request(
                "evaluateMoves",
                base_params(
                    "rollout",
                    die1=3,
                    die2=1,
                    levelOptions={"trials": 36, "maxMoves": 2, "shortlistPlies": 0},
                ),
                timeout=900,
            )
        )
        alternatives = result.get("Alternatives") or []
        expect(len(alternatives) == 2, f"maxMoves honoured: {len(alternatives)} alternatives")
        expect(
            alternatives[0]["MoveNotation"] == "8/5 6/5",
            f"3-1 rolls out to the five point, got {alternatives[0]['MoveNotation']}",
        )

    check("rollout evaluateMoves", rollout_moves_small)

    def error_paths():
        response = client.request("evaluatePosition", base_params("7ply"))
        expect(response["error"]["code"] == -32001, f"unknown level: {response}")
        response = client.request(
            "evaluatePosition",
            {"positionId": "invalid!", "matchId": MONEY_MATCH_ID, "level": "0ply"},
        )
        expect(response["error"]["code"] == -32002, f"invalid id: {response}")

    check("error codes", error_paths)

    def shutdown():
        result_of(client.request("shutdown"))
        process.wait(timeout=30)
        expect(process.returncode == 0, f"exit code {process.returncode}")

    check("shutdown", shutdown)

    if failures:
        print(f"E2E FAILED: {failures}")
        sys.exit(1)
    print("E2E OK")


if __name__ == "__main__":
    main()
