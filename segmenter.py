"""Pause-aware boundaries for mono signed 16-bit, 16 kHz PCM."""
from array import array
import sys

RATE = 16000
FRAME = RATE * 2 // 50  # 20 ms
MIN_BYTES = RATE * 2 * 4
MAX_BYTES = RATE * 2 * 7
QUIET_FRAMES = 12  # 240 ms
RMS_THRESHOLD = 30


class Segmenter:
    def __init__(self):
        self.buffer = bytearray()
        self.scanned = 0
        self.quiet = 0

    def feed(self, data):
        self.buffer.extend(data)
        chunks = []
        while self.scanned + FRAME <= len(self.buffer):
            samples = array('h', self.buffer[self.scanned:self.scanned + FRAME])
            if sys.byteorder != 'little':
                samples.byteswap()
            self.quiet = self.quiet + 1 if sum(x * x for x in samples) / len(samples) < RMS_THRESHOLD ** 2 else 0
            self.scanned += FRAME
            boundary = None
            if self.scanned >= MIN_BYTES and self.quiet >= QUIET_FRAMES:
                # Split in the middle of the quiet interval; retain every sample.
                boundary = max(MIN_BYTES, self.scanned - self.quiet * FRAME // 2)
            elif self.scanned >= MAX_BYTES:
                boundary = MAX_BYTES
            if boundary is not None:
                chunks.append(bytes(self.buffer[:boundary]))
                del self.buffer[:boundary]
                self.scanned = 0
                self.quiet = 0
        return chunks

    def finish(self):
        tail = bytes(self.buffer[:len(self.buffer) // 2 * 2])
        self.buffer.clear()
        self.scanned = self.quiet = 0
        return tail


def is_quiet(pcm):
    """Every short frame is below the pause threshold; isolated peaks are OK."""
    if not pcm or len(pcm) % 2:
        return False
    for offset in range(0, len(pcm), FRAME):
        samples = array('h', pcm[offset:offset + FRAME])
        if sys.byteorder != 'little':
            samples.byteswap()
        if sum(x * x for x in samples) / len(samples) >= RMS_THRESHOLD ** 2:
            return False
    return True
