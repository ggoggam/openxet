"""Self-check for the Xet chunk-frame decoder (mirrors crates/cas_types)."""

import lz4.frame

from openxet_fsspec.client import _ungroup4, decode_chunks


def _group4(data: bytes) -> bytes:
    """Forward BG4 grouping (test-only inverse of _ungroup4)."""
    n = len(data)
    full, rem = divmod(n, 4)
    groups = [bytearray() for _ in range(4)]
    idx = 0
    while idx + 3 < n:
        for g in range(4):
            groups[g].append(data[idx + g])
        idx += 4
    for g in range(4):
        if idx + g < n:
            groups[g].append(data[idx + g])
    assert [len(g) for g in groups] == [full + (1 if g < rem else 0) for g in range(4)]
    return b"".join(groups)


def _frame(raw: bytes, ctype: int) -> bytes:
    if ctype == 0:
        payload = raw
    elif ctype == 1:
        payload = lz4.frame.compress(raw)
    else:
        payload = lz4.frame.compress(_group4(raw))
    header = (
        bytes([0])
        + len(payload).to_bytes(3, "little")
        + bytes([ctype])
        + len(raw).to_bytes(3, "little")
    )
    return header + payload


def test_ungroup4_roundtrip() -> None:
    for n in range(9):  # exercise all remainder cases
        data = bytes(range(40, 40 + n))
        assert _ungroup4(_group4(data)) == data


def test_decode_chunks_all_compression_types_and_range() -> None:
    chunks = [
        bytes([i]) * (100 + i) for i in range(3)
    ]  # odd lengths hit bg4 remainders
    data = b"".join(_frame(c, ctype) for c, ctype in zip(chunks, (0, 1, 2)))
    assert decode_chunks(data, 0, 3) == b"".join(chunks)
    assert decode_chunks(data, 1, 2) == chunks[1]  # skip + stop-early
