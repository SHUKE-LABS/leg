#!/usr/bin/env python3
"""Calculate the predeclared paired TUI/Web trial scores using Python 3 only."""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from pathlib import Path
from typing import Any


INTERFACES = ("TUI", "Web")
TASKS = (
    "compose_send_multiline",
    "continue",
    "find_prior_session",
    "identify_running_tool",
    "inspect_failed_or_denied_tool",
    "stop_running_tool",
    "reopen_after_failure_or_interruption",
    "explicit_retry",
    "browse_copy_long_answer",
)
COMMON_GATES = (
    "common_critical_workflows_pass",
    "no_accidental_submit_or_replay",
    "committed_history_preserved",
    "stop_terminates_tool_processes",
    "credentials_not_exposed",
)
WEB_GATE = "unauthenticated_web_mutations_rejected"


class InputError(ValueError):
    pass


def _positive_ms(value: Any, where: str, *, allow_zero: bool = False) -> int | None:
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int):
        raise InputError(f"{where} must be an integer millisecond value or null")
    minimum = 0 if allow_zero else 1
    if value < minimum:
        raise InputError(f"{where} must be at least {minimum} ms or null")
    return value


def _validate_measurement(value: Any, where: str, *, setup: bool = False) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise InputError(f"{where} must be an object")
    required = ("success", "elapsed_ms") if setup else ("success", "elapsed_ms", "provider_wait_ms", "ease")
    missing = [field for field in required if field not in value]
    if missing:
        raise InputError(f"{where} must include {', '.join(missing)}; use null for missing values")
    success = value.get("success")
    if success is not None and not isinstance(success, bool):
        raise InputError(f"{where}.success must be true, false, or null")
    elapsed = _positive_ms(value.get("elapsed_ms"), f"{where}.elapsed_ms")
    provider_wait = None
    ease = None
    if not setup:
        provider_wait = _positive_ms(value.get("provider_wait_ms"), f"{where}.provider_wait_ms", allow_zero=True)
        ease = value.get("ease")
        if ease is not None and (isinstance(ease, bool) or not isinstance(ease, int) or not 1 <= ease <= 7):
            raise InputError(f"{where}.ease must be an integer from 1 through 7 or null")
    return {"success": success, "elapsed_ms": elapsed, "provider_wait_ms": provider_wait, "ease": ease}


def validate(data: Any) -> dict[str, Any]:
    if not isinstance(data, dict) or data.get("schema") != "leg-ui-trial.results/v1":
        raise InputError("input schema must be leg-ui-trial.results/v1")
    participants = data.get("participants")
    if not isinstance(participants, list):
        raise InputError("participants must be an array")
    normalized: list[dict[str, Any]] = []
    ids: set[str] = set()
    for index, participant in enumerate(participants):
        where = f"participants[{index}]"
        if not isinstance(participant, dict):
            raise InputError(f"{where} must be an object")
        participant_id = participant.get("id")
        if not isinstance(participant_id, str) or not participant_id.strip():
            raise InputError(f"{where}.id must be a non-empty anonymous id")
        if participant_id in ids:
            raise InputError(f"duplicate participant id: {participant_id}")
        ids.add(participant_id)
        if "leg_unfamiliar" not in participant:
            raise InputError(f"{where}.leg_unfamiliar must be recorded as true, false, or null")
        unfamiliar = participant.get("leg_unfamiliar")
        if unfamiliar is not None and not isinstance(unfamiliar, bool):
            raise InputError(f"{where}.leg_unfamiliar must be true, false, or null")
        attempted = participant.get("full_script_attempted", {})
        if not isinstance(attempted, dict):
            raise InputError(f"{where}.full_script_attempted must be an object")
        missing_interfaces = [interface for interface in INTERFACES if interface not in attempted]
        if missing_interfaces:
            raise InputError(f"{where}.full_script_attempted must include {', '.join(missing_interfaces)}")
        attempted_by_interface = {}
        for interface in INTERFACES:
            value = attempted.get(interface, False)
            if not isinstance(value, bool):
                raise InputError(f"{where}.full_script_attempted.{interface} must be boolean")
            attempted_by_interface[interface] = value

        setup_input = participant.get("setup", {})
        tasks_input = participant.get("tasks", {})
        if not isinstance(setup_input, dict) or not isinstance(tasks_input, dict):
            raise InputError(f"{where}.setup and tasks must be objects")
        if any(interface not in setup_input for interface in INTERFACES):
            raise InputError(f"{where}.setup must include both TUI and Web measurements")
        if any(task not in tasks_input for task in TASKS):
            raise InputError(f"{where}.tasks must include all nine task measurements")
        setup = {
            interface: _validate_measurement(setup_input.get(interface, {}), f"{where}.setup.{interface}", setup=True)
            for interface in INTERFACES
        }
        tasks: dict[str, dict[str, dict[str, Any]]] = {}
        for task in TASKS:
            task_input = tasks_input.get(task, {})
            if not isinstance(task_input, dict):
                raise InputError(f"{where}.tasks.{task} must be an object")
            if any(interface not in task_input for interface in INTERFACES):
                raise InputError(f"{where}.tasks.{task} must include both TUI and Web measurements")
            tasks[task] = {
                interface: _validate_measurement(task_input.get(interface, {}), f"{where}.tasks.{task}.{interface}")
                for interface in INTERFACES
            }
        normalized.append(
            {
                "id": participant_id,
                "leg_unfamiliar": unfamiliar,
                "full_script_attempted": attempted_by_interface,
                "setup": setup,
                "tasks": tasks,
            }
        )
    return {"participants": normalized, "eligibility_gates": data.get("eligibility_gates", {})}


def _successful(measurement: dict[str, Any]) -> bool:
    return measurement["success"] is True


def _active_ms(measurement: dict[str, Any]) -> int | None:
    elapsed = measurement["elapsed_ms"]
    wait = measurement["provider_wait_ms"]
    if elapsed is None or wait is None:
        return None
    return max(1, elapsed - wait)


def _paired_ratio(left: dict[str, Any], right: dict[str, Any], *, setup: bool = False) -> tuple[float, float]:
    left_ok = _successful(left)
    right_ok = _successful(right)
    if left_ok and not right_ok:
        measured = left["elapsed_ms"] is not None if setup else _active_ms(left) is not None
        return (1.0, 0.0) if measured else (0.0, 0.0)
    if right_ok and not left_ok:
        measured = right["elapsed_ms"] is not None if setup else _active_ms(right) is not None
        return (0.0, 1.0) if measured else (0.0, 0.0)
    if not left_ok and not right_ok:
        return (0.0, 0.0)
    left_time = left["elapsed_ms"] if setup else _active_ms(left)
    right_time = right["elapsed_ms"] if setup else _active_ms(right)
    if left_time is None or right_time is None:
        return (0.0, 0.0)
    return (min(left_time, right_time) / max(1, left_time), min(left_time, right_time) / max(1, right_time))


def _eligible(gates: Any, interface: str) -> tuple[bool, list[str]]:
    if not isinstance(gates, dict):
        return (False, ["eligibility gates missing"])
    candidate = gates.get(interface)
    if not isinstance(candidate, dict):
        return (False, ["eligibility gates missing"])
    required = list(COMMON_GATES)
    if interface == "Web":
        required.append(WEB_GATE)
    reasons = [key for key in required if candidate.get(key) is not True]
    if interface == "TUI" and candidate.get(WEB_GATE) != "n/a":
        reasons.append(f"{WEB_GATE} must be marked n/a for TUI")
    return (not reasons, reasons)


def calculate(raw: Any) -> dict[str, Any]:
    data = validate(raw)
    participants = data["participants"]
    n = len(participants)
    per_interface: dict[str, dict[str, Any]] = {}
    task_medians: dict[str, dict[str, float]] = {}
    raw_totals: dict[str, float] = {}

    for task in TASKS:
        ratios = {interface: [] for interface in INTERFACES}
        for participant in participants:
            left, right = (participant["tasks"][task][interface] for interface in INTERFACES)
            left_ratio, right_ratio = _paired_ratio(left, right)
            ratios["TUI"].append(left_ratio)
            ratios["Web"].append(right_ratio)
        task_medians[task] = {
            interface: statistics.median(ratios[interface]) if ratios[interface] else 0.0
            for interface in INTERFACES
        }

    setup_ratios = {interface: [] for interface in INTERFACES}
    for participant in participants:
        left_ratio, right_ratio = _paired_ratio(
            participant["setup"]["TUI"], participant["setup"]["Web"], setup=True
        )
        setup_ratios["TUI"].append(left_ratio)
        setup_ratios["Web"].append(right_ratio)

    for interface in INTERFACES:
        successes = 0
        ease_values: list[float] = []
        for participant in participants:
            for task in TASKS:
                measurement = participant["tasks"][task][interface]
                if _successful(measurement):
                    successes += 1
                    ease = measurement["ease"]
                    ease_values.append((ease - 1) / 6 if ease is not None else 0.0)
                else:
                    ease_values.append(0.0)
        completion_fraction = successes / (len(TASKS) * n) if n else 0.0
        ease_mean = sum(ease_values) / (len(TASKS) * n) if n else 0.0
        time_mean = sum(task_medians[task][interface] for task in TASKS) / len(TASKS)
        setup_median = statistics.median(setup_ratios[interface]) if setup_ratios[interface] else 0.0
        raw_points = {
            "completion": 40 * completion_fraction,
            "time": 25 * time_mean,
            "ease": 25 * ease_mean,
            "setup": 10 * setup_median,
        }
        raw_totals[interface] = sum(raw_points.values())
        points = {key: round(value, 2) for key, value in raw_points.items()}
        points["total"] = round(raw_totals[interface], 2)
        eligible, failed_gates = _eligible(data["eligibility_gates"], interface)
        per_interface[interface] = {
            "points": points,
            "eligibility_pass": eligible,
            "failed_gates": failed_gates,
            "setup_median_paired_speed_ratio": round(setup_median, 6),
        }

    complete_pairs = [
        participant
        for participant in participants
        if participant["full_script_attempted"]["TUI"] and participant["full_script_attempted"]["Web"]
    ]
    unfamiliar_pairs = sum(participant["leg_unfamiliar"] is True for participant in complete_pairs)
    gate_reasons = []
    if len(complete_pairs) < 5:
        gate_reasons.append(f"need at least 5 paired full-script participants; have {len(complete_pairs)}")
    if len(complete_pairs) < n:
        gate_reasons.append(f"all included participants must attempt both full scripts; {n - len(complete_pairs)} incomplete")
    if unfamiliar_pairs < 2:
        gate_reasons.append(f"need at least 2 paired participants unfamiliar with leg; have {unfamiliar_pairs}")

    recommendation: dict[str, Any]
    if gate_reasons:
        recommendation = {"status": "withheld", "candidate": None, "reasons": gate_reasons}
    else:
        eligible = [interface for interface in INTERFACES if per_interface[interface]["eligibility_pass"]]
        if len(eligible) == 1:
            recommendation = {"status": "recommend", "candidate": eligible[0], "reasons": ["exactly one candidate passed eligibility gates"]}
        elif not eligible:
            recommendation = {"status": "shuke_decides", "candidate": None, "reasons": ["neither candidate passed all eligibility gates"]}
        else:
            tui_score = raw_totals["TUI"]
            web_score = raw_totals["Web"]
            gap = abs(tui_score - web_score)
            if gap < 5 - 1e-9:
                recommendation = {"status": "shuke_decides", "candidate": None, "reasons": [f"eligible scores differ by {gap:.2f} points, less than 5"]}
            else:
                candidate = "TUI" if tui_score > web_score else "Web"
                recommendation = {"status": "recommend", "candidate": candidate, "reasons": [f"eligible score gap is {gap:.2f} points"]}

    return {
        "schema": "leg-ui-trial.score-report/v1",
        "participant_count": n,
        "paired_full_script_count": len(complete_pairs),
        "paired_full_script_leg_unfamiliar_count": unfamiliar_pairs,
        "scores": per_interface,
        "per_task_median_paired_speed_ratios": {
            task: {interface: round(value, 6) for interface, value in values.items()}
            for task, values in task_medians.items()
        },
        "recommendation": recommendation,
    }


def _measurement(success: bool | None, elapsed: int | None, wait: int | None = 0, ease: int | None = 7) -> dict[str, Any]:
    return {"success": success, "elapsed_ms": elapsed, "provider_wait_ms": wait, "ease": ease}


def _participant(identifier: str, unfamiliar: bool, tui: dict[str, Any] | None = None, web: dict[str, Any] | None = None) -> dict[str, Any]:
    tui = tui or {}
    web = web or {}
    return {
        "id": identifier,
        "leg_unfamiliar": unfamiliar,
        "full_script_attempted": {"TUI": True, "Web": True},
        "setup": {
            "TUI": {"success": True, "elapsed_ms": 1000},
            "Web": {"success": True, "elapsed_ms": 1000},
        },
        "tasks": {
            task: {
                "TUI": tui.get(task, _measurement(True, 1000)),
                "Web": web.get(task, _measurement(True, 1000)),
            }
            for task in TASKS
        },
    }


def self_check() -> None:
    gates = {
        "TUI": {**{key: True for key in COMMON_GATES}, WEB_GATE: "n/a"},
        "Web": {**{key: True for key in COMMON_GATES}, WEB_GATE: True},
    }
    base = {
        "schema": "leg-ui-trial.results/v1",
        "eligibility_gates": gates,
        "participants": [_participant("P01", True)],
    }
    perfect = calculate(base)
    if perfect["scores"]["TUI"]["points"] != {"completion": 40.0, "time": 25.0, "ease": 25.0, "setup": 10.0, "total": 100.0}:
        raise AssertionError("perfect-case score should be 100 points")
    if perfect["recommendation"]["status"] != "withheld":
        raise AssertionError("one pair must not produce a recommendation")

    # The failed first task remains in the denominator. The paired time ratio
    # is also retained as 0/1 for that pair, rather than dropping the row.
    failed = _participant(
        "P02",
        False,
        tui={"compose_send_multiline": _measurement(False, 12000, 0, None)},
    )
    partial = {**base, "participants": [_participant("P01", True), failed]}
    report = calculate(partial)
    if report["scores"]["TUI"]["points"]["completion"] != round(40 * 17 / 18, 2):
        raise AssertionError("failed tasks must contribute zero within the full 9-task denominator")
    if report["scores"]["TUI"]["points"]["ease"] != round(25 * 17 / 18, 2):
        raise AssertionError("failed tasks must contribute zero to ease within the full denominator")
    if report["per_task_median_paired_speed_ratios"]["compose_send_multiline"] != {"TUI": 0.5, "Web": 1.0}:
        raise AssertionError("failed paired task must remain in the per-task median")

    missing = _participant(
        "P03",
        False,
        tui={"compose_send_multiline": _measurement(None, None, None, None)},
    )
    missing_report = calculate({**base, "participants": [_participant("P01", True), missing]})
    if missing_report["scores"]["TUI"]["points"]["completion"] != round(40 * 17 / 18, 2):
        raise AssertionError("a missing task result must remain in the completion denominator as zero")
    if missing_report["scores"]["TUI"]["points"]["ease"] != round(25 * 17 / 18, 2):
        raise AssertionError("a missing task result must contribute zero ease within the full denominator")
    if missing_report["per_task_median_paired_speed_ratios"]["compose_send_multiline"] != {"TUI": 0.5, "Web": 1.0}:
        raise AssertionError("a missing task result must remain in the per-task time median as zero")

    active = _paired_ratio(_measurement(True, 10000, 9000), _measurement(True, 2000, 0))
    if active != (1.0, 0.5):
        raise AssertionError("task speed must subtract provider waiting before comparing")
    missing_time = _paired_ratio(_measurement(True, None, None), _measurement(False, 1000, 0))
    if missing_time != (0.0, 0.0):
        raise AssertionError("a missing successful-task timing must contribute zero to time")
    malformed = _participant("M01", True)
    del malformed["tasks"][TASKS[0]]["TUI"]["provider_wait_ms"]
    try:
        calculate({**base, "participants": [malformed]})
    except InputError:
        pass
    else:
        raise AssertionError("missing raw fields must be explicitly recorded as null")
    floor = _paired_ratio(_measurement(True, 1000, 999), _measurement(True, 2, 0))
    if floor != (1.0, 0.5):
        raise AssertionError("provider wait subtraction must use a 1 ms floor")

    five = [_participant(f"P{i:02}", i < 3) for i in range(1, 6)]
    exact_one = {**base, "participants": five, "eligibility_gates": {**gates, "Web": {**gates["Web"], WEB_GATE: False}}}
    one_report = calculate(exact_one)
    if one_report["recommendation"]["candidate"] != "TUI":
        raise AssertionError("exactly one eligible candidate must be recommended after the paired gate")

    gap_data = [_participant(f"P{i:02}", i < 3) for i in range(1, 6)]
    for participant in gap_data:
        for task in TASKS[:2]:
            participant["tasks"][task]["TUI"] = _measurement(True, 1)
            participant["tasks"][task]["Web"] = _measurement(True, 10)
    gap_report = calculate({**base, "participants": gap_data})
    if gap_report["recommendation"]["status"] != "recommend" or gap_report["recommendation"]["candidate"] != "TUI":
        raise AssertionError("eligible candidates with a 5-point score gap should recommend the higher score")

    below_gap_data = [_participant(f"Q{i:02}", i < 3) for i in range(1, 6)]
    for participant in below_gap_data:
        for task in TASKS[:2]:
            participant["tasks"][task]["TUI"] = _measurement(True, 1)
            participant["tasks"][task]["Web"] = _measurement(True, 7)
    below_gap = calculate({**base, "participants": below_gap_data})
    if below_gap["recommendation"]["status"] != "shuke_decides":
        raise AssertionError("score gaps under 5 points must remain for shuke to decide")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", nargs="?", type=Path, help="results JSON file")
    parser.add_argument("--self-check", action="store_true", help="run built-in score-calculator checks")
    args = parser.parse_args()
    if args.self_check:
        self_check()
        print("score calculator self-check: PASS")
        return 0
    if args.input is None:
        parser.error("provide a result JSON path or --self-check")
    try:
        with args.input.open(encoding="utf-8") as stream:
            raw = json.load(stream)
        report = calculate(raw)
    except (OSError, json.JSONDecodeError, InputError) as error:
        print(f"score calculator: {error}", file=sys.stderr)
        return 2
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
