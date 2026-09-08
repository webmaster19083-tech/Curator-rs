import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

class WorkerEncodingTest(unittest.TestCase):
    def test_unicode_path_with_legacy_pipe_encoding(self):
        worker = Path(__file__).resolve().parents[1] / 'nsfw_worker.py'
        with tempfile.TemporaryDirectory() as tmp:
            folder = Path(tmp)
            media = folder / 'emoji-\U0001f621-\u4e2d\u6587.jpg'
            media.write_bytes(b'fixture')
            (folder / 'opennsfw_onnx.py').write_text('from pathlib import Path\nfrom types import SimpleNamespace\nclass NSFWClassifier:\n def warmup(self): pass\n def classify(self, path):\n  assert Path(path).read_bytes() == b"fixture"\n  return SimpleNamespace(nsfw=0.25)\n')
            env = dict(os.environ, PYTHONPATH=tmp, PYTHONIOENCODING='cp1252', PYTHONUTF8='0')
            result = subprocess.run([sys.executable, str(worker)], input=(json.dumps({'id': 1, 'path': str(media)}, ensure_ascii=False)+'\n').encode('utf-8'), capture_output=True, env=env, timeout=15, check=True)
            replies = [json.loads(line) for line in result.stdout.splitlines()]
            self.assertEqual(replies, [{'ready': True}, {'id': 1, 'score': 0.25}])

if __name__ == '__main__':
    unittest.main()
