import json
import tempfile
import unittest
from array import array
from pathlib import Path
from unittest.mock import patch
import segmenter as s
import sessions
import recover
import voice


def tone(seconds, amplitude=4000):
    data = array('h', [amplitude, -amplitude] * int(s.RATE * seconds / 2))
    import sys
    if sys.byteorder != 'little':
        data.byteswap()
    return data.tobytes()


class SegmentationTests(unittest.TestCase):
    def split(self, pcm, block=8191):
        segmenter = s.Segmenter()
        parts = []
        for offset in range(0, len(pcm), block):
            parts.extend(segmenter.feed(pcm[offset:offset + block]))
        if tail := segmenter.finish():
            parts.append(tail)
        self.assertEqual(b''.join(parts), pcm)
        return parts

    def test_pause_boundary(self):
        pcm = tone(4.5) + bytes(int(.4 * s.RATE * 2)) + tone(2)
        parts = self.split(pcm)
        self.assertEqual(len(parts), 2)
        boundary_seconds = len(parts[0]) / (s.RATE * 2)
        self.assertGreater(boundary_seconds, 4.5)
        self.assertLess(boundary_seconds, 4.9)

    def test_continuous_speech_is_bounded(self):
        parts = self.split(tone(16))
        self.assertEqual([len(p) / (s.RATE * 2) for p in parts], [7, 7, 2])

    def test_quiet_speech_is_not_treated_as_silence(self):
        self.assertEqual([len(p) / (s.RATE * 2) for p in self.split(tone(8, amplitude=100))], [7, 1])

    def test_short_recording_stays_whole(self):
        self.assertEqual(len(self.split(tone(2.7))), 1)

    def test_reader_block_size_does_not_change_boundaries(self):
        pcm = tone(4.5) + bytes(s.RATE) + tone(8)
        self.assertEqual(self.split(pcm, 2000), self.split(pcm, 8191))

    def test_recovery_uses_saved_boundaries_and_pending_tail(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(sessions, 'ROOT', Path(tmp)):
            path = sessions.create()
            first = tone(4.5) + bytes(int(.12 * s.RATE * 2))
            tail = tone(2)
            (path / 'audio.pcm').write_bytes(first + tail)
            sessions.write(path / 'boundaries.json', [len(first)])
            sessions.write(path / 'chunk-000000.json', {'text': 'Начало.', 'usage': {}})
            with patch.object(voice, 'transcribe', return_value=('Конец.', {})) as api:
                self.assertEqual(recover.resume(path, 'key'), 'Начало. Конец.')
                self.assertEqual(api.call_count, 1)
            self.assertEqual(json.loads((path / 'boundaries.json').read_text()), [len(first), len(first + tail)])

    def test_bad_manifest_does_not_send_audio(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(sessions, 'ROOT', Path(tmp)):
            path = sessions.create()
            (path / 'audio.pcm').write_bytes(tone(1))
            sessions.write(path / 'boundaries.json', [10000000])
            with patch.object(voice, 'transcribe') as api:
                with self.assertRaises(ValueError):
                    recover.resume(path, 'key')
                api.assert_not_called()


if __name__ == '__main__':
    unittest.main()
