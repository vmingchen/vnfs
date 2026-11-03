#!/usr/bin/env python3
"""Use exactly the bounded du fixture generator, including its identity manifest."""
from pathlib import Path
import runpy
runpy.run_path(str(Path(__file__).resolve().parents[1]/"du/generate.py"), run_name="__main__")
