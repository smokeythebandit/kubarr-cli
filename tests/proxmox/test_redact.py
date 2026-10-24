import base64
import io
import json
import unittest
import zipfile
from redact import EMBEDDED_ZIP, scrub


class ReportRedactionTests(unittest.TestCase):
    def test_embedded_report_step_arguments_are_scrubbed_without_breaking_zip(self):
        buffer = io.BytesIO()
        with zipfile.ZipFile(buffer, 'w', zipfile.ZIP_DEFLATED) as archive:
            archive.writestr('steps.json', json.dumps({'title': 'Fill PRIVATE_password'}))
            archive.writestr('summary.json', '{"passed":true}')
        html = b'<script>"data:application/zip;base64,' + base64.b64encode(buffer.getvalue()) + b'"</script>'
        result = scrub(html, [b'PRIVATE_password'])
        encoded = EMBEDDED_ZIP.search(result).group(1)
        with zipfile.ZipFile(io.BytesIO(base64.b64decode(encoded))) as archive:
            self.assertEqual(json.loads(archive.read('steps.json')), {'title': 'Fill [REDACTED]'})
            self.assertEqual(json.loads(archive.read('summary.json')), {'passed': True})

    def test_raw_logs_and_zip_reports_are_scrubbed(self):
        self.assertEqual(scrub(b'url/#PRIVATE_token', [b'PRIVATE_token']), b'url/#[REDACTED]')
        buffer = io.BytesIO()
        with zipfile.ZipFile(buffer, 'w') as archive:
            archive.writestr('steps.json', 'PRIVATE_password')
        with zipfile.ZipFile(io.BytesIO(scrub(buffer.getvalue(), [b'PRIVATE_password']))) as archive:
            self.assertEqual(archive.read('steps.json'), b'[REDACTED]')

    def test_request_error_session_tokens_are_scrubbed_without_known_cookie_values(self):
        token = b'eyJ0ZXN0IjoxfQ.eyJzaWQiOiJ0ZXN0In0.signature'
        self.assertEqual(scrub(b'cookie: kubarr_session_0=' + token, []), b'cookie: kubarr_session_0=[REDACTED_SESSION]')
