"""Exercise the server-independent facade benchmark with small fixtures."""

import json
import runpy
import sys
import tempfile
from pathlib import Path

BENCHMARK = Path(__file__).resolve().parents[1] / "benchmarks" / "facade.py"


def test_facade_benchmark_reports_all_cases(monkeypatch, tmp_path, capsys):
    monkeypatch.setattr(tempfile, "tempdir", str(tmp_path))
    monkeypatch.setattr(
        sys,
        "argv",
        [str(BENCHMARK), "--files", "3", "--bytes", "64", "--rounds", "2"],
    )
    namespace = runpy.run_path(str(BENCHMARK), run_name="__main__")
    report = json.loads(capsys.readouterr().out)
    assert set(report["results"]) == set(namespace["CASES"])
    for case in report["results"].values():
        assert case["wall_ns_median"] > 0
        assert case["cpu_ns_median"] > 0
        assert case["rpcs_median"] is None
        assert len(case["samples"]) == 2


def test_facade_readinto_fills_buffers_larger_than_one_read(
    monkeypatch, tmp_path, capsys
):
    monkeypatch.setattr(tempfile, "tempdir", str(tmp_path))
    monkeypatch.setattr(
        sys,
        "argv",
        [
            str(BENCHMARK),
            "--cases",
            "readinto",
            "--files",
            "2",
            "--bytes",
            str((1 << 20) + 1),
            "--rounds",
            "1",
            "--warmups",
            "0",
        ],
    )
    runpy.run_path(str(BENCHMARK), run_name="__main__")
    report = json.loads(capsys.readouterr().out)
    assert len(report["results"]["readinto"]["samples"]) == 1
