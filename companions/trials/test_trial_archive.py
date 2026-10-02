from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path

from companions.trials.make_trial_archive import create_archive


class TrialArchiveTests(unittest.TestCase):
    def test_archive_bytes_ignore_source_timestamps(self) -> None:
        with tempfile.TemporaryDirectory() as root_string:
            root = Path(root_string)
            source_a = root / "source-a"
            source_b = root / "source-b"
            source_a.mkdir()
            source_b.mkdir()
            for source, timestamp in ((source_a, 1_000_000_000), (source_b, 1_700_000_000)):
                (source / "README.md").write_text("same content\n", encoding="utf-8")
                (source / "nested").mkdir()
                (source / "nested" / "run.sh").write_text("#!/bin/sh\ntrue\n", encoding="utf-8")
                (source / "nested" / "run.sh").chmod(0o755)
                for path in (source, source / "README.md", source / "nested", source / "nested" / "run.sh"):
                    os.utime(path, (timestamp, timestamp))

            archive_a = root / "a.tar.gz"
            archive_b = root / "b.tar.gz"
            create_archive(source_a, archive_a, "bundle")
            create_archive(source_b, archive_b, "bundle")
            self.assertEqual(archive_a.read_bytes(), archive_b.read_bytes())


if __name__ == "__main__":
    unittest.main()
