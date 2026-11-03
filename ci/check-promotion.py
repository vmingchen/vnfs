#!/usr/bin/env python3
"""Fail closed unless CI and the exact four-port gate passed on one main commit."""
import argparse
import json
import re
import subprocess


def verify(repository, commit):
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("commit must be a full lowercase Git SHA")
    evidence = {}
    for workflow in ("ci.yml", "ports.yml"):
        runs = json.loads(subprocess.check_output([
            "gh", "run", "list", "--repo", repository, "--workflow", workflow,
            "--branch", "main", "--commit", commit, "--limit", "100",
            "--json", "databaseId,headSha,headBranch,status,conclusion,event,createdAt,url",
        ], text=True, timeout=60))
        events = {"push"} if workflow == "ci.yml" else {"push", "workflow_dispatch", "schedule"}
        eligible = [r for r in runs if r["headSha"] == commit and r["headBranch"] == "main" and r["event"] in events]
        if not eligible:
            raise ValueError(f"{workflow}: no same-commit main run")
        latest = max(eligible, key=lambda r: (r["createdAt"], r["databaseId"]))
        if latest["status"] != "completed" or latest["conclusion"] != "success":
            raise ValueError(f"{workflow}: latest run is {latest['status']}/{latest['conclusion']}: {latest['url']}")
        evidence[workflow] = latest
    return {"commit": commit, "checks": evidence}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("commit")
    parser.add_argument("--repo", default="vmingchen/vnfs")
    args = parser.parse_args()
    try:
        print(json.dumps(verify(args.repo, args.commit), indent=2))
    except (ValueError, KeyError, subprocess.SubprocessError) as error:
        parser.exit(1, f"Mirror promotion refused: {error}\n")
