#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Copy each `component-type*` custom section of a core module to a section named
# `starling:` followed by the original name.
#
# The `component-type*` sections hold the worlds of the module's wit-bindgen
# bindings. `wasm-tools component new` consumes and removes them, and the
# component's own type keeps only the interfaces the module imports functions
# from. Linking wit-dylib bindings against the core module again needs the
# complete worlds: a stream intrinsic such as
# `[stream-read-0]read-via-stream` is resolved through the type of the function
# it names, which the module need not import. `starling-componentize` restores
# the copies when it takes the core module out of the component.
#
# Usage: preserve-component-type.py <core module>
# The module is rewritten in place. A module that already has the copies is left
# unchanged.

import sys
from pathlib import Path

SEC_CUSTOM = 0
SOURCE_PREFIX = b"component-type"
COPY_PREFIX = b"starling:"


class Truncated(Exception):
    """A LEB128 number or a section runs past the end of its section or module."""


def read_leb(data, pos, end):
    """Read an unsigned LEB128 number at `pos`, which must end before `end`."""
    result = shift = 0
    while True:
        if pos >= end:
            raise Truncated
        byte = data[pos]
        pos += 1
        result |= (byte & 0x7F) << shift
        shift += 7
        if byte < 0x80:
            return result, pos


def leb(value):
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def custom_section(name, payload):
    body = leb(len(name)) + name + payload
    return bytes([SEC_CUSTOM]) + leb(len(body)) + body


def main():
    path = Path(sys.argv[1])
    data = path.read_bytes()
    if data[:4] != b"\0asm" or data[4:8] != b"\x01\0\0\0":
        sys.exit(f"{path}: not a core wasm module")

    copies = []
    pos = 8
    try:
        while pos < len(data):
            section_id = data[pos]
            size, body = read_leb(data, pos + 1, len(data))
            end = body + size
            if end > len(data):
                raise Truncated
            if section_id == SEC_CUSTOM:
                name_len, name_start = read_leb(data, body, end)
                if name_start + name_len > end:
                    raise Truncated
                name = data[name_start:name_start + name_len]
                if name.startswith(COPY_PREFIX):
                    print(f"{path}: already has the {COPY_PREFIX.decode()} copies")
                    return
                if name.startswith(SOURCE_PREFIX):
                    copies.append(custom_section(COPY_PREFIX + name, data[name_start + name_len:end]))
            pos = end
    except Truncated:
        sys.exit(f"{path}: truncated core wasm module")

    if not copies:
        sys.exit(f"{path}: no component-type sections")
    path.write_bytes(data + b"".join(copies))
    print(f"{path}: copied {len(copies)} component-type section(s)")


if __name__ == "__main__":
    main()
