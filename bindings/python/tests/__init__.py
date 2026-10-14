"""Tests for the Python binding.

Run from this directory::

    cargo build -p openfield-bot-sidecar --features testkit
    cargo build -p openfield-bot-ffi
    python -m unittest discover -v -t .

The sidecar must carry the ``testkit`` feature, because the suite drives the Rust mock
server rather than a second mock written in Python; see :mod:`.support`.
"""
