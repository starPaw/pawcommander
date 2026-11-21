"""Hold selected mutation replies so the test can close its SSH transport."""

import os
from pathlib import Path
import struct
import subprocess
import sys


LSTAT = 7
SETSTAT = 9
REMOVE = 13
RENAME = 18
STATUS = 101
BEFORE = {"lost-before.bin", "lost-permissions-before.bin"}
AFTER = {
    "lost-after.bin", "lost-move-dir", "lost-replacement.bin", "lost-conflict.bin",
    "lost-held-after", "lost-remove.bin", "lost-permissions-after.bin",
}
PERMISSIONS_READBACK = "lost-permissions-readback.bin"


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


def path_name(packet):
    size, = struct.unpack_from(">I", packet, 5)
    return Path(os.fsdecode(packet[9:9 + size])).name


def mutation_name(packet):
    if packet[0] == REMOVE:
        name = path_name(packet)
        return name if name == "lost-remove.bin" else None
    if packet[0] == SETSTAT:
        name = path_name(packet)
        if name in {"lost-permissions-before.bin", "lost-permissions-after.bin", PERMISSIONS_READBACK}:
            return name
    return rename_target(packet)


def run(fixture, sftp_server):
    with subprocess.Popen(
        [sftp_server], stdin=subprocess.PIPE, stdout=subprocess.PIPE
    ) as backend:
        permissions_changed = False
        try:
            while (request := read_packet(sys.stdin.buffer)) is not None:
                name = mutation_name(request)
                readback = permissions_changed and request[0] == LSTAT and path_name(request) == PERMISSIONS_READBACK
                if readback:
                    name = PERMISSIONS_READBACK
                marker = fixture / ("held-" + name) if readback or name in BEFORE | AFTER else None
                hold = marker is not None and not marker.exists()
                if hold and (readback or name in BEFORE):
                    condition = "readback" if readback else "before"
                else:
                    write_packet(backend.stdin, request)
                    response = read_packet(backend.stdout)
                    if response is None:
                        raise RuntimeError("The SFTP server returned no response")
                    if request[0] == SETSTAT and name == PERMISSIONS_READBACK:
                        if response[0] != STATUS or response[1:5] != request[1:5] or response[5:9] != b"\0" * 4:
                            raise RuntimeError("The disposable server did not set permissions")
                        permissions_changed = True
                    if not hold:
                        write_packet(sys.stdout.buffer, response)
                        continue
                    if response[0] != STATUS or response[1:5] != request[1:5] or response[5:9] != b"\0" * 4:
                        raise RuntimeError("The disposable server did not apply the mutation")
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
