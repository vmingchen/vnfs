"""Exercise benchmark argument preflight without creating files or sessions."""

import runpy
import sys
from pathlib import Path

import pytest


@pytest.mark.parametrize("workers, valid", [(2, True), (17, False)])
def test_pipeline_budget_uses_effective_worker_count(
    monkeypatch, tmp_path, workers, valid
):
    benchmark = Path(__file__).resolve().parents[1] / "benchmarks" / "large_file.py"

    class SetupReached(Exception):
        pass

    def stop_before_mkdir(*args, **kwargs):
        raise SetupReached

    monkeypatch.setattr(
        sys,
        "argv",
        [
            str(benchmark),
            "--fixture-root",
            str(tmp_path),
            "--pipeline-workers",
            str(workers),
            "--in-flight",
            "32",
            "--chunk-kib",
            "1024",
        ],
    )
    monkeypatch.setattr(Path, "mkdir", stop_before_mkdir)
    if valid:
        with pytest.raises(SetupReached):
            runpy.run_path(str(benchmark), run_name="__main__")
    else:
        with pytest.raises(SystemExit) as error:
            runpy.run_path(str(benchmark), run_name="__main__")
        assert error.value.code == 2
