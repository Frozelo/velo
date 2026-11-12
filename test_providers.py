import io
import json
import os
import tempfile
import unittest
import urllib.error
from pathlib import Path
from unittest.mock import patch
import providers


class RoutingTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.state = patch.object(providers, 'STATE', Path(self.tmp.name))
        self.state.start()
        self.env = patch.dict(os.environ, {'VELO_PROVIDER': 'auto'})
        self.env.start()
        self.keys = patch.object(providers, 'key', return_value='pv-test')
        self.keys.start()

    def tearDown(self):
        self.state.stop()
        self.env.stop()
        self.keys.stop()
        self.tmp.cleanup()

    def build(self, provider):
        return b'{}', 'application/json', provider + '-model'

    def response(self, cost=.01):
        return io.BytesIO(json.dumps({'text': 'ok', 'usage': {'cost': cost}}).encode())

    def test_primary_success_does_not_send_to_fallback(self):
        with patch.object(providers.urllib.request, 'urlopen', return_value=self.response()) as call:
            result = providers.request('/audio/transcriptions', self.build, 'or-test')
            self.assertEqual(call.call_count, 1)
            self.assertEqual(result['usage']['provider'], 'openrouter')
            self.assertEqual(result['usage']['cost'], .01)

    def test_network_failure_uses_separate_key_and_circuit(self):
        with patch.object(providers.urllib.request, 'urlopen', side_effect=[urllib.error.URLError('offline'), self.response()]) as call:
            result = providers.request('/audio/transcriptions', self.build, 'or-test')
            self.assertEqual(call.call_args_list[0].args[0].get_header('Authorization'), 'Bearer or-test')
            self.assertEqual(call.call_args_list[1].args[0].get_header('Authorization'), 'Bearer pv-test')
            self.assertEqual(result['usage']['provider'], 'plusvibe')
            self.assertNotIn('cost', result['usage'])
            self.assertTrue(result['usage']['prior_attempt_cost_unknown'])
        with patch.object(providers.urllib.request, 'urlopen', return_value=self.response()) as call:
            providers.request('/chat/completions', self.build, 'or-test')
            self.assertTrue(call.call_args.args[0].full_url.startswith(providers.BASES['plusvibe']))
            self.assertEqual(call.call_count, 1)

    def test_auth_error_is_not_silently_hidden(self):
        error = urllib.error.HTTPError('url', 401, 'auth', {}, None)
        with patch.object(providers.urllib.request, 'urlopen', side_effect=error) as call:
            with self.assertRaises(urllib.error.HTTPError):
                providers.request('/chat/completions', self.build, 'or-test')
            self.assertEqual(call.call_count, 1)

    def test_region_denial_uses_fallback(self):
        error = urllib.error.HTTPError('url', 403, 'region', {}, None)
        with patch.object(providers.urllib.request, 'urlopen', side_effect=[error, self.response()]):
            self.assertEqual(providers.request('/chat/completions', self.build, 'or-test')['usage']['provider'], 'plusvibe')

    def test_only_plusvibe_never_sends_sentinel(self):
        with patch.object(providers.urllib.request, 'urlopen', return_value=self.response()) as call:
            providers.request('/chat/completions', self.build, 'plusvibe-only')
            self.assertEqual(call.call_args.args[0].get_header('Authorization'), 'Bearer pv-test')
            self.assertTrue(call.call_args.args[0].full_url.startswith(providers.BASES['plusvibe']))

    def test_forced_primary_does_not_use_secondary(self):
        with patch.dict(os.environ, {'VELO_PROVIDER': 'openrouter'}), patch.object(providers.urllib.request, 'urlopen', side_effect=urllib.error.URLError('offline')) as call:
            with self.assertRaises(urllib.error.URLError):
                providers.request('/chat/completions', self.build, 'or-test')
            self.assertEqual(call.call_count, 1)


if __name__ == '__main__':
    unittest.main()
