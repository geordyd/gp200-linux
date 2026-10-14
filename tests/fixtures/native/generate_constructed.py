"""Independent stdlib fixtures from the captured GP-200 record stream.

These are constructed from the recovered Windows file writer, NOT official
exports. The separately downloaded upstream .prst is never redistributed.
"""
from pathlib import Path
import hashlib
import json
import struct

ROOT = Path(__file__).resolve().parents[3]
OUT = Path(__file__).resolve().parent
capture = ROOT / 'assets/demo/read-current-payload.bin'
b = capture.read_bytes()
records = []
offset = 0
while offset < len(b):
    command, size = struct.unpack_from('<HH', b, offset)
    end = offset + 4 + size
    assert end <= len(b)
    if command not in (0, 1):
        records.append(b[offset:end])
    offset = end
payload = b''.join(records)
assert len(payload) == 1168 and len(records) == 33

def container(payload, ir=None):
    mode = 1 if ir is None else 2
    header = bytearray(32)
    header[:4] = b'TSRP'
    header[16:20] = b'2-PG'
    struct.pack_into('<HH', header, 20, 0x0100, mode)
    header[24:32] = bytes.fromhex('a5a55a5a01234567')
    if ir is None:
        directory = struct.pack('<II', 40, len(payload) + 4)
        chunks = b''
    else:
        directory = struct.pack('<IIII', 48, len(ir), 56 + len(ir), len(payload) + 4)
        chunks = b'RISU' + struct.pack('<I', len(ir)) + ir
    data = bytes(header) + directory + chunks + b'MRAP' + struct.pack('<I', len(payload) + 4) + payload
    data += struct.pack('<I', len(data))
    data += struct.pack('>I', sum(data) & 0xffffffff)
    return data

fixture_specs = {
    'constructed-mode1.prst': (container(payload), 'Constructed mode 1; hardware-captured raw stream, opaque header bytes.'),
    'constructed-mode2.prst': (container(payload, bytes(range(256)) * 7), 'Constructed mode 2; synthetic opaque RISU bytes, not a genuine User IR.'),
}
manifest = {
    'kind': 'independent-constructed-fixtures-not-official-export',
    'generator': 'generate_constructed.py',
    'raw_source': str(capture.relative_to(ROOT)),
    'raw_source_sha256': hashlib.sha256(b).hexdigest(),
    'source_executable_sha256': 'c06e9b1634b8dbb3f44d1726c691fc497867b977601d3ad67b02dc025a87ceb5',
    'writer_address': '0x5dfab0',
    'fixtures': [],
}
for name, (data, description) in fixture_specs.items():
    (OUT / name).write_bytes(data)
    manifest['fixtures'].append({'path': name, 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest(), 'additive_checksum': sum(data[:-4]) & 0xffffffff, 'description': description})
(OUT / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
