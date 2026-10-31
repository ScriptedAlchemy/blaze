#!/usr/bin/env python3
"""Prune only older cache entries after observing a replacement on the exact ref.

A successful actions/cache step can be a warning/no-op. Neither that outcome nor
an empty API filter is authority to delete cache entries. No tokens are logged.
"""
import datetime
import json
import os
import re
import subprocess


def superseded(pages, key, scope, ref):
    if not re.fullmatch(r"sccache-v1-[A-Za-z0-9_.-]+-(Linux|macOS|Windows)-(X64|ARM64|ARM|X86)-", scope):
        raise ValueError("invalid sccache scope")
    if not key.startswith(scope) or not re.fullmatch(r"[0-9a-f]{16}-[0-9a-f]{40}", key[len(scope):]):
        raise ValueError("cache key does not belong to the supplied scope")
    if not re.fullmatch(r"refs/(heads/[^\s?&#]+|pull/[0-9]+/merge)", ref):
        raise ValueError("invalid cache ref")
    if not isinstance(pages, list) or not pages:
        raise ValueError("missing cache response pages")
    entries = []
    for page in pages:
        if not isinstance(page, dict) or not isinstance(page.get("actions_caches"), list):
            raise ValueError("incomplete cache response")
        entries.extend(page["actions_caches"])
    rows = {}
    for row in entries:
        if not isinstance(row, dict):
            raise ValueError("invalid cache entry")
        cache_id = row.get("id")
        if type(cache_id) is not int or cache_id <= 0:
            raise ValueError("invalid cache identity")
        if cache_id in rows and rows[cache_id] != row:
            raise ValueError("conflicting cache identity")
        rows[cache_id] = row

    def created(row):
        try:
            value = datetime.datetime.fromisoformat(row.get("created_at", "").replace("Z", "+00:00"))
            return value if value.tzinfo is not None else None
        except (ValueError, TypeError, AttributeError):
            return None

    replacements = {}
    for row in rows.values():
        version = row.get("version")
        when = created(row)
        if (row.get("key") == key and row.get("ref") == ref
                and type(row.get("size_in_bytes")) is int and row["size_in_bytes"] > 0
                and isinstance(version, str) and version and when is not None):
            replacements[version] = max(when, replacements.get(version, when))
    # An absent/empty replacement or unknown version retains every old cache.
    doomed = []
    for cache_id, row in rows.items():
        row_key = row.get("key")
        when = created(row)
        replacement = replacements.get(row.get("version")) if isinstance(row.get("version"), str) else None
        if (row.get("ref") == ref and isinstance(row_key, str) and row_key != key
                and row_key.startswith(scope)
                and re.fullmatch(r"[0-9a-f]{16}-[0-9a-f]{40}", row_key[len(scope):])
                and replacement is not None
                and when is not None and when < replacement):
            doomed.append(cache_id)
    return sorted(doomed)


def main():
    key, scope, ref = (os.environ.get(name, "") for name in ("KEY", "SCOPE", "REF"))
    try:
        # Validate before making even a listing request with potentially empty filters.
        superseded([{"actions_caches": []}], key, scope, ref)
        repo = os.environ["GITHUB_REPOSITORY"]
        raw = subprocess.check_output([
            "gh", "api", "--method", "GET", "--paginate", "--slurp",
            f"repos/{repo}/actions/caches", "-f", f"ref={ref}", "-f", f"key={scope}",
            "-F", "per_page=100"], text=True)
        doomed = superseded(json.loads(raw), key, scope, ref)
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError):
        print("::warning::Cache retention skipped: no complete, validated replacement inventory")
        return
    for cache_id in doomed:
        result = subprocess.run(["gh", "api", "--method", "DELETE",
                                 f"repos/{repo}/actions/caches/{cache_id}"],
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        print(f"{'Deleted' if result.returncode == 0 else 'Retained'} superseded cache {cache_id}")


if __name__ == "__main__":
    main()
