#!/usr/bin/env python3
"""Strictly parse exactly one JSON record and reject duplicate object keys."""
import json
import pathlib
import sys


class InvalidRecord(ValueError):
    pass


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise InvalidRecord(f"duplicate object key: {key}")
        result[key] = value
    return result


def load_record(path):
    raw = pathlib.Path(path).read_text(encoding="utf-8")
    decoder = json.JSONDecoder(object_pairs_hook=unique_object)
    try:
        value, end = decoder.raw_decode(raw)
    except (json.JSONDecodeError, InvalidRecord) as error:
        raise InvalidRecord(str(error)) from error
    if raw[end:].strip():
        raise InvalidRecord("trailing or concatenated JSON value")
    return value


def main():
    if len(sys.argv) < 2:
        print(f"usage: {sys.argv[0]} JSON_RECORD...", file=sys.stderr)
        return 2
    try:
        for name in sys.argv[1:]:
            load_record(name)
    except (InvalidRecord, OSError, UnicodeError) as error:
        print(f"invalid JSON record: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
