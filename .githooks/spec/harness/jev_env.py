"""Resolve the jev endpoint for canon harnesses — one contract, two sources.

Two env conventions exist on real machines and they are NOT interchangeable:
  - MCP convention: JEV_API_KEY + JEV_API_BASE_URL (base ends in /v1)
  - legacy direct:  TYPESAFE_API_KEY + TYPESAFE_API_BASE

A configured-but-dead TYPESAFE_API_BASE (observed: DNS failure while the MCP
endpoint works) silently degraded every jev tier to candidates-raw/INFO —
the worst failure mode, green-looking checks with no judgment behind them.
So: an endpoint is only usable as a *pair* (key+base), and the MCP pair wins
when present. Callers must treat key=None as "no jev tier" and degrade loudly.
"""

from __future__ import annotations

import os


def resolve() -> tuple[str | None, str | None, str]:
    """Return (base, key, model); POST target is {base}/v1/systemone; key None = no tier.

    JEV_API_BASE_URL on real machines carries either a bare host, a /v1
    base, or the full /systemone path — accept all three spellings.
    """
    model = os.environ.get("JEV_MODEL", "jev-latest")
    if os.environ.get("JEV_API_KEY"):
        base, key = os.environ.get("JEV_API_BASE_URL", ""), os.environ["JEV_API_KEY"]
    elif os.environ.get("TYPESAFE_API_KEY"):
        base, key = (
            os.environ.get("TYPESAFE_API_BASE", "https://api.typesafe.ai"),
            os.environ["TYPESAFE_API_KEY"],
        )
    else:
        return None, None, model
    base = (base or "").rstrip("/")
    if not base:
        return None, key, model
    base = base.removesuffix("/systemone")
    base = base.removesuffix("/v1")
    return base, key, model
