"""Hold one selected rename reply so the test can close its SSH transport."""

import os
from pathlib import Path
import struct
import subprocess
import sys


RENAME = 18
STATUS = 101
BEFORE = {"lost-before.bin"}
AFTER = {"lost-after.bin", "lost-move-dir", "lost-replacement.bin", "lost-conflict.bin", "lost-held-after"}


def read_packet(stream):
    header = stream.read(4)
    if not header:
        return None
    size, = struct.unpack(">I", header)
    if not 1 <= size <= 1024 * 1024:
        raise RuntimeError("Unexpected SFTP packet length")
    packet = stream.read(size)
    if len(packet) != size:
        raise RuntimeError("Truncated SFTP packet")
    return packet


def write_packet(stream, packet):
    stream.write(struct.pack(">I", len(packet)))
    stream.write(packet)
    stream.flush()


def rename_target(packet):
    if packet[0] != RENAME:
        return None
    source_length, = struct.unpack_from(">I", packet, 5)
    target_offset = 9 + source_length
    target_length, = struct.unpack_from(">I", packet, target_offset)
    start = target_offset + 4
    target = Path(os.fsdecode(packet[start:start + target_length]))
    if target.name == "source" and target.parent.name.startswith(".pawcommander-sftp-"):
        source = Path(os.fsdecode(packet[9:target_offset]))
        if source.name == "lost-held-after":
            return source.name
    return target.name


def run(fixture, sftp_server):
    with subprocess.Popen(
        [sftp_server], stdin=subprocess.PIPE, stdout=subprocess.PIPE
    ) as backend:
        try:
            while (request := read_packet(sys.stdin.buffer)) is not None:
                name = rename_target(request)
                marker = fixture / ("held-" + name) if name in BEFORE | AFTER else None
                hold = marker is not None and not marker.exists()
                if hold and name in BEFORE:
                    condition = "before"
                else:
                    write_packet(backend.stdin, request)
                    response = read_packet(backend.stdout)
                    if response is None:
                        raise RuntimeError("The SFTP server returned no response")
                    if not hold:
                        write_packet(sys.stdout.buffer, response)
                        continue
                    if response[0] != STATUS or response[1:5] != request[1:5] or response[5:9] != b"\0" * 4:
                        raise RuntimeError("The disposable server did not apply the rename")
                    condition = "after"
                staged = marker.with_suffix(".fixture")
                staged.write_text(condition, encoding="ascii")
                staged.replace(marker)
                while sys.stdin.buffer.read(4096):
                    pass
                break
        finally:
            backend.stdin.close()


if __name__ == "__main__":
    run(Path(sys.argv[1]), sys.argv[2])
