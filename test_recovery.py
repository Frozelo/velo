import io
import os
import signal
import chunked
import json
import tempfile
import unittest
import urllib.error
from pathlib import Path
from unittest.mock import patch
import sessions
import recover
import voice
import cost_stats
from chunked import CHUNK_BYTES


class RecoveryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = patch.object(sessions, 'ROOT', Path(self.tmp.name) / 'sessions')
        self.root.start()
        self.cost = patch.object(cost_stats, 'FILE', Path(self.tmp.name) / 'costs.jsonl')
        self.cost.start()
        self.cost_dir = patch.object(cost_stats, 'DIR', Path(self.tmp.name))
        self.cost_dir.start()
        self.path = sessions.create()

    def tearDown(self):
        self.root.stop()
        self.cost.stop()
        self.cost_dir.stop()
        self.tmp.cleanup()

    def test_resume_only_missing_chunks_in_order(self):
        (self.path / 'audio.pcm').write_bytes(bytes(CHUNK_BYTES + 32000))
        sessions.write(self.path / 'chunk-000000.json', {'text': 'Первая часть.', 'usage': {'cost': .001}})
        with patch.object(voice, 'transcribe', return_value=('Вторая часть.', {'cost': .002})) as api:
            self.assertEqual(recover.resume(self.path, 'dummy'), 'Первая часть. Вторая часть.')
            self.assertEqual(api.call_count, 1)
            self.assertEqual(recover.resume(self.path, 'dummy'), 'Первая часть. Вторая часть.')
            self.assertEqual(api.call_count, 1)

    def test_failed_request_preserves_audio_and_success(self):
        (self.path / 'audio.pcm').write_bytes(bytes(CHUNK_BYTES + 1000))
        sessions.write(self.path / 'chunk-000000.json', {'text': 'Готовая часть', 'usage': {}})
        with patch.object(voice, 'transcribe', side_effect=urllib.error.HTTPError('url', 401, 'unauthorized', {}, io.BytesIO())) as api:
            with self.assertRaises(urllib.error.HTTPError):
                recover.resume(self.path, 'dummy')
            self.assertEqual(api.call_count, 1)
        self.assertTrue((self.path / 'audio.pcm').exists())
        self.assertTrue((self.path / 'chunk-000000.json').exists())
        self.assertFalse((self.path / 'raw.json').exists())

    def test_transient_retry(self):
        with patch('sessions.time.sleep'), patch.object(voice, 'transcribe', side_effect=[urllib.error.URLError('offline'), ('Готово', {'cost': .001})]) as api:
            self.assertEqual(sessions.transcribe(self.path, bytes(1000), 0, 'dummy')[0], 'Готово')
            self.assertEqual(api.call_count, 2)
        self.assertTrue((self.path / 'attempts-000000.jsonl').exists())
        self.assertFalse(sessions.account(self.path, 'disabled')['complete'])

    def test_session_lock(self):
        with sessions.lock(self.path):
            with self.assertRaises(RuntimeError):
                with sessions.lock(self.path):
                    pass

    def test_finish_keeps_text_removes_audio(self):
        (self.path / 'audio.pcm').write_bytes(bytes(100))
        sessions.save_text(self.path, 'Результат')
        sessions.finish(self.path)
        self.assertFalse((self.path / 'audio.pcm').exists())
        self.assertEqual(json.loads((self.path / 'raw.json').read_text())['text'], 'Результат')

    def test_cleanup_skips_active_session(self):
        with sessions.lock(self.path):
            self.assertEqual(sessions.cleanup(hours=0), 0)
        self.assertEqual(sessions.cleanup(hours=0), 1)

    def test_recording_failure_is_recoverable(self):
        read_fd, write_fd = os.pipe()
        os.write(write_fd, bytes(1000))
        os.close(write_fd)
        handlers = {}
        class Process:
            stdout = os.fdopen(read_fd, 'rb')
            def poll(self):
                return 0
            def wait(self):
                handlers[signal.SIGTERM](signal.SIGTERM, None)
            def send_signal(self, value):
                pass
        process = Process()
        with patch.object(voice, 'STATE', Path(self.tmp.name) / 'runtime'), patch.object(chunked.signal, 'signal', side_effect=lambda sig, fn: handlers.update({sig: fn})), patch.object(voice, 'key', return_value='dummy'), patch.object(voice, 'focused_window', return_value=None), patch.object(chunked.subprocess, 'Popen', return_value=process), patch.object(voice, 'transcribe', side_effect=ValueError('bad response')):
            self.assertEqual(chunked.run(False), 1)
        process.stdout.close()
        saved = [p.parent for p in sessions.ROOT.glob('*/session.json') if p.parent != self.path][0]
        self.assertEqual((saved / 'audio.pcm').stat().st_size, 1000)
        self.assertEqual(json.loads((saved / 'session.json').read_text())['status'], 'failed')
        with patch.object(voice, 'transcribe', return_value=('Восстановлено', {})):
            self.assertEqual(recover.resume(saved, 'dummy'), 'Восстановлено')

    def test_permissions(self):
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o700)
        self.assertEqual((self.path / 'session.json').stat().st_mode & 0o777, 0o600)


if __name__ == '__main__':
    unittest.main()
