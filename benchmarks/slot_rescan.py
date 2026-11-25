"""Compare the round-robin scan of synthetic live slots between two Git revisions."""

import argparse
import shutil
import subprocess
from pathlib import Path
from statistics import median
from tempfile import TemporaryDirectory


def implementation(revision, masked):
    source_path = "crates/victory-gpu/src/descriptor/host.rs"
    source = subprocess.check_output(["git", "show", f"{revision}:{source_path}"], text=True)
    code = source[source.index("struct Slots("):source.index("/// The record each slot")]
    scan = "self.0.part::<16>(part)" if masked else "self.0.all(|slot| slot % 16 == part)"
    return code + """
pub struct Sample(Slots);
impl Sample {
    pub fn new(count: usize) -> Self { Self(Slots::new(count)) }
    pub fn put(&self, slot: u32) { self.0.put(slot); }
    pub fn part(&self, part: u32) -> Vec<u32> { SCAN }
}
""".replace("SCAN", scan)


def run(base, new):
    with TemporaryDirectory() as directory:
        temp = Path(directory)
        for name, revision, masked in [("before", base, False), ("after", new, True)]:
            (temp / (name + ".rs")).write_text(implementation(revision, masked), encoding="utf-8")
        shutil.copyfile(Path(__file__).with_suffix(".rs"), temp / "main.rs")
        subprocess.run(
            ["rustc", "--edition=2024", "-O", "-A", "dead_code", "main.rs", "-o", "bench"],
            cwd=temp, check=True,
        )
        rows = subprocess.check_output([str(temp / "bench")], cwd=temp, text=True).splitlines()
    for live in ("83000", "128"):
        for masked in ("false", "true"):
            times = [
                int(micros) / 1000 for count, _, mode, micros in (row.split() for row in rows)
                if count == live and mode == masked
            ]
            print(f"{live} live, masked={masked}, median {median(times):.3f} ms for eight rounds")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base")
    parser.add_argument("new")
    args = parser.parse_args()
    run(args.base, args.new)
