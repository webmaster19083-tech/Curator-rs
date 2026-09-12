import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

class WorkerEncodingTest(unittest.TestCase):
    def test_unicode_path_with_utf8_jsonl_encoding(self):
        worker = Path(__file__).resolve().parents[1] / 'nsfw_worker.py'
        with tempfile.TemporaryDirectory() as tmp:
            folder = Path(tmp)
            media = folder / 'emoji-\U0001f621-\u4e2d\u6587.jpg'
            media.write_bytes(b'fixture')
            (folder / 'nudenet.py').write_text('''__version__ = "fixture"\nclass NudeDetector:\n def detect_batch(self, paths):\n  from pathlib import Path\n  assert Path(paths[0]).read_bytes() == b"fixture"\n  return [[{"class": "BELLY_EXPOSED", "score": 0.25, "box": [1, 2, 3, 4]}] for _ in paths]\n''')
            env = dict(os.environ, PYTHONPATH=tmp, PYTHONIOENCODING='cp1252', PYTHONUTF8='0')
            result = subprocess.run([sys.executable, str(worker)], input=(json.dumps({'id': 1, 'paths': [str(media)]}, ensure_ascii=False)+'\n').encode('utf-8'), capture_output=True, env=env, timeout=15, check=True)
            replies = [json.loads(line) for line in result.stdout.splitlines()]
            self.assertEqual(replies[0], {'ready': True, 'model': 'NudeNet-320', 'version': 'fixture'})
            self.assertEqual(replies[1]['id'], 1)
            self.assertEqual(replies[1]['results'][0]['detections'][0]['label'], 'BELLY_EXPOSED')
            self.assertEqual(replies[1]['results'][0]['detections'][0]['box'], [1, 2, 3, 4])

if __name__ == '__main__':
    unittest.main()
