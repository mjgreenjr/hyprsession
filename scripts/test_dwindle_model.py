#!/usr/bin/env python3
"""Tests for dwindle_model.py, using only synthetic data taken from the v1.2 spike #1 fixture."""
import json, os, subprocess, sys, tempfile, unittest


def dump(obj, path):
    with open(path, "w") as f:
        json.dump(obj, f)


HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import dwindle_model as m  # noqa: E402

BOX = (0, 26, 1920, 1080)
# Spike #1 fixture "A" (insertion order d1 d2 d3 d4, pointer in the bottom-left region)
S4 = [("d1", (12, 38, 953, 546)), ("d2", (967, 38, 1908, 1068)),
      ("d3", (12, 560, 478, 1068)), ("d4", (492, 560, 953, 1068))]


def clients(entries, ws=12):
    return [{"workspace": {"id": ws}, "floating": False, "at": [r[0], r[1]],
             "size": [r[2] - r[0], r[3] - r[1]], "argv": ["foot", f"--working-directory=/x/{t}"]}
            for t, r in entries]


class Model(unittest.TestCase):
    def test_pointer_rule_reproduces_the_measured_split_targets(self):
        targets, tree, _ = m.simulate(4, "pointer", [(300, 900)] * 3, BOX)
        self.assertEqual(targets, [1, 1, 3])  # spike #1: step 2 splits #1, step 3 #1, step 4 #3
        self.assertEqual(tree, ("V", ("H", ("L", 1), ("V", ("L", 3), ("L", 4))), ("L", 2)))

    def test_other_rules_do_not_reproduce_them(self):
        for rule in ("oldest", "newest", "largest", "smallest"):
            self.assertNotEqual(m.simulate(4, rule, None, BOX)[0], [1, 1, 3], rule)

    def test_reconstruct_the_fixture(self):
        tree = m.reconstruct([(i, r) for i, (_, r) in enumerate(S4)])
        self.assertEqual(tree, ("V", ("H", ("L", 0), ("V", ("L", 2), ("L", 3))), ("L", 1)))

    def test_computed_order_for_the_original_shape_is_the_build_order(self):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "c.json")
            dump(clients(S4), p)
            out = subprocess.run([sys.executable, os.path.join(HERE, "dwindle_model.py"), "order", p, "12", "pointer",
                                  "--pointer", "300,900"], capture_output=True, text=True)
        lines = out.stdout.splitlines()
        self.assertEqual(out.returncode, 0, out.stdout)
        self.assertTrue(lines[0].startswith("MODE computed"))
        self.assertEqual([l.split("/")[-1] for l in lines[1:]], ["d1", "d2", "d3", "d4"])

    def test_mirror_shape_is_unreachable_without_steering_and_falls_back_to_reading_order(self):
        # full-height LEFT + stack on the right: the user's usual pattern
        mirror = [("a", (12, 38, 953, 1068)), ("b", (967, 38, 1908, 546)), ("c", (967, 560, 1908, 1068))]
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "c.json")
            dump(clients(mirror), p)
            out = subprocess.run([sys.executable, os.path.join(HERE, "dwindle_model.py"), "order", p, "12", "pointer",
                                  "--pointer", "300,900"], capture_output=True, text=True)
        self.assertEqual(out.returncode, 3)
        self.assertTrue(out.stdout.startswith("MODE fallback"))
        # reading order: a (x 12), b (967,38), c (967,560)
        self.assertEqual([l.split("/")[-1] for l in out.stdout.splitlines()[1:]], ["a", "b", "c"])

    def test_pointer_steering_reaches_the_mirror_shape(self):
        mirror = [("a", (12, 38, 953, 1068)), ("b", (967, 38, 1908, 546)), ("c", (967, 560, 1908, 1068))]
        tree = m.reconstruct([(i, r) for i, (_, r) in enumerate(mirror)])
        # simulate with the pointers steer would use: step 2 anywhere (right half), step 3 in the right half
        targets, simtree, _ = m.simulate(3, "pointer", [(1500, 500), (1500, 500)], BOX)
        self.assertEqual(targets, [1, 2])
        mapping = {}
        self.assertTrue(m.match(simtree, tree, mapping))
        self.assertEqual(mapping, {0: 1, 1: 2, 2: 3})

    def test_steer_output_lists_one_pointer_per_insertion_after_the_first(self):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "c.json")
            mirror = [("a", (12, 38, 953, 1068)), ("b", (967, 38, 1908, 546)), ("c", (967, 560, 1908, 1068))]
            dump(clients(mirror), p)
            out = subprocess.run([sys.executable, os.path.join(HERE, "dwindle_model.py"), "steer", p, "12"],
                                 capture_output=True, text=True).stdout.splitlines()
        self.assertEqual(out[0], "MODE steer")
        self.assertEqual(len([l for l in out if l.startswith("POINTER")]), 2)
        self.assertEqual([l.split("/")[-1] for l in out[1:4]], ["a", "b", "c"])

    def test_floating_and_other_workspaces_are_ignored(self):
        cs = clients(S4) + [{"workspace": {"id": 3}, "floating": False, "at": [0, 0], "size": [5, 5], "argv": ["x"]},
                            {"workspace": {"id": 12}, "floating": True, "at": [0, 0], "size": [5, 5], "argv": ["y"]}]
        self.assertEqual(len(m.rects_from_clients(cs, 12)), 4)

    def test_derive_and_score_a_recorded_run(self):
        with tempfile.TemporaryDirectory() as d:
            run = os.path.join(d, "runs", "r1")
            os.makedirs(run)
            order = ["d1", "d2", "d3", "d4"]
            steps = [
                {"d1": (12, 38, 1896, 1030)},
                {"d1": (12, 38, 941, 1030), "d2": (967, 38, 941, 1030)},
                {"d1": (12, 38, 941, 508), "d2": (967, 38, 941, 1030), "d3": (12, 560, 941, 508)},
                {"d1": (12, 38, 941, 508), "d2": (967, 38, 941, 1030), "d3": (12, 560, 466, 508), "d4": (492, 560, 461, 508)},
            ]
            for k, table in enumerate(steps, 1):
                with open(os.path.join(run, f"t{k}.tsv"), "w") as f:
                    for t, (x, y, w, h) in table.items():
                        f.write(f"{t}\t{x}\t{y}\t{w}\t{h}\n")
            dump({"label": "r1", "order": order, "pointers": [[300, 900]] * 3,
                  "box": [0, 26, 1920, 1080]}, os.path.join(run, "meta.json"))
            subprocess.run([sys.executable, os.path.join(HERE, "dwindle_model.py"), "derive", run], check=True,
                           capture_output=True)
            with open(os.path.join(d, "runs", "rule.jsonl")) as f:
                recorded = json.loads(f.readline())
            self.assertEqual(recorded["targets"], [1, 1, 3])
            out = subprocess.run([sys.executable, os.path.join(HERE, "dwindle_model.py"), "score",
                                  os.path.join(d, "runs", "rule.jsonl")], capture_output=True, text=True).stdout
            self.assertIn("BEST pointer", out)

if __name__ == "__main__":
    unittest.main()
