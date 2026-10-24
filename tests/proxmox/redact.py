"""Remove ephemeral credentials from Playwright HTML report step arguments."""
import base64
import io
from pathlib import Path
import re
import zipfile

EMBEDDED_ZIP = re.compile(rb'data:application/zip;base64,([A-Za-z0-9+/=]+)')
JWT = re.compile(rb'eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+')


def scrub(data, secrets):
    def archive_bytes(payload):
        result = io.BytesIO()
        with zipfile.ZipFile(io.BytesIO(payload)) as source, zipfile.ZipFile(result, 'w') as target:
            for entry in source.infolist():
                content = source.read(entry.filename)
                for secret in secrets:
                    content = content.replace(secret, b'[REDACTED]')
                content = JWT.sub(b'[REDACTED_SESSION]', content)
                target.writestr(entry, content)
        return result.getvalue()

    data = EMBEDDED_ZIP.sub(lambda match: b'data:application/zip;base64,' + base64.b64encode(
        archive_bytes(base64.b64decode(match.group(1)))), data)
    if data.startswith(b'PK\x03\x04'):
        data = archive_bytes(data)
    for secret in secrets:
        data = data.replace(secret, b'[REDACTED]')
    return JWT.sub(b'[REDACTED_SESSION]', data)


def redact_artifacts(root, values, skip_logs=False):
    secrets = [value.encode() for value in values if value]
    for path in Path(root).rglob('*'):
        if not path.is_file() or path.suffix in ('.png', '.webm'):
            continue
        if skip_logs and path.suffix == '.log':
            continue
        before = path.read_bytes()
        after = scrub(before, secrets)
        if after != before:
            path.write_bytes(after)
