#!/usr/bin/env python3
"""Dwindle growth model for the v1.2 spike (standard library only).

Hyprland's dwindle layout grows a binary tree: inserting a window splits one existing leaf into
(old occupant, new window). The new window goes second (right / bottom, as with force_split=2)
and the split runs along the leaf box's longer side. WHICH leaf is split is the open question the
spike measures; this module holds candidate rules ("hypotheses"), scores them against measured
runs, and, for a chosen rule, computes the insertion order that grows a saved tree exactly.

Geometry is topological: windows are compared by tree shape (orientation and child order), not by
pixel equality, because gaps and rounding make rectangles differ by a few pixels.

Subcommands (all print to stdout; exit code 3 means "no exact order exists"):
  derive RUNDIR              analyse a recorded run (tables t<k>.tsv + meta.json), append to rule.jsonl
  score  RULE_JSONL          score every hypothesis against all recorded runs
  tree   CLIENTS_JSON WS     print the tree reconstructed from a snapshot's tiled windows on WS
  order  CLIENTS_JSON WS RULE [--pointer X,Y]
                             print "MODE computed|fallback ..." then the windows in insertion order
  steer  CLIENTS_JSON WS     print, per insertion step, the leaf box a pointer must be inside
                             (pointer-steered growth of an arbitrary tree), same line format as order
"""
import json
import math
import os
import sys

OUTER_GAP = 12  # gaps_out, used to recover the layout box from window rectangles


# ------------------------------------------------------------------ geometry / trees

def split_box(box):
    """Split a node box along its longer side. Returns (orientation, first, second).
    'V' = side by side (first is left), 'H' = stacked (first is top)."""
    x0, y0, x1, y1 = box
    if (x1 - x0) > (y1 - y0):
        xm = (x0 + x1) / 2
        return "V", (x0, y0, xm, y1), (xm, y0, x1, y1)
    ym = (y0 + y1) / 2
    return "H", (x0, y0, x1, ym), (x0, ym, x1, y1)


def distance(point, box):
    """Distance from a point to a box (0 inside)."""
    px, py = point
    x0, y0, x1, y1 = box
    dx = max(x0 - px, 0, px - x1)
    dy = max(y0 - py, 0, py - y1)
    return math.hypot(dx, dy)


class Leaf:
    def __init__(self, ident, box):
        self.id = ident
        self.box = box


def choose(rule, leaves, step, pointer):
    """The hypotheses: which leaf does insertion number `step` split?"""
    if rule == "oldest":       # always the first window
        return min(leaves, key=lambda l: l.id)
    if rule == "newest":       # always the previous arrival
        return max(leaves, key=lambda l: l.id)
    if rule == "pointer":      # the leaf nearest the pointer (inside = 0); ties: oldest
        if pointer is None:
            return None
        return min(leaves, key=lambda l: (distance(pointer, l.box), l.id))
    if rule == "largest":
        return min(leaves, key=lambda l: (-(l.box[2] - l.box[0]) * (l.box[3] - l.box[1]), l.id))
    if rule == "smallest":
        return min(leaves, key=lambda l: ((l.box[2] - l.box[0]) * (l.box[3] - l.box[1]), l.id))
    raise SystemExit(f"unknown rule {rule}")


RULES = ["oldest", "newest", "pointer", "largest", "smallest"]


def simulate(n, rule, pointers=None, box=(0, 26, 1920, 1080)):
    """Grow n windows. `pointers[k-2]` is the pointer at insertion k (k >= 2).
    Returns (targets, tree, boxes) where targets[k-2] is the insertion index split at step k,
    tree is nested ('L', id) / (orientation, first, second), boxes maps id -> final box."""
    root = ["L", 1, box]
    leaves = {1: root}  # id -> node (list so it can be mutated into an internal node)
    targets = []
    for k in range(2, n + 1):
        pointer = pointers[k - 2] if pointers and k - 2 < len(pointers) else None
        candidates = [Leaf(i, node[2]) for i, node in leaves.items()]
        target = choose(rule, candidates, k, pointer)
        if target is None:
            return None
        node = leaves[target.id]
        orient, first, second = split_box(node[2])
        old = ["L", target.id, first]
        new = ["L", k, second]
        node[:] = [orient, old, new]
        leaves[target.id] = old
        leaves[k] = new
        targets.append(target.id)
    return targets, freeze(root), {i: node[2] for i, node in leaves.items()}


def freeze(node):
    if node[0] == "L":
        return ("L", node[1])
    return (node[0], freeze(node[1]), freeze(node[2]))


def rects_from_clients(clients, ws):
    """Tiled windows of a snapshot on workspace ws as (index, argv, rect) in file order."""
    out = []
    for c in clients:
        if c["workspace"]["id"] == ws and not c.get("floating") and c.get("argv") is not None:
            x, y = c["at"]
            w, h = c["size"]
            out.append((len(out), c["argv"], (x, y, x + w, y + h)))
    return out


def reconstruct(items, tol=24):
    """Rebuild the guillotine tree of window rectangles. items: [(key, rect)]. Returns nested
    ('L', key) / (orientation, first, second) or None if the rectangles are not a dwindle partition."""
    def rec(group):
        if len(group) == 1:
            return ("L", group[0][0])
        x0 = min(r[0] for _, r in group); y0 = min(r[1] for _, r in group)
        x1 = max(r[2] for _, r in group); y1 = max(r[3] for _, r in group)
        prefer = "V" if (x1 - x0) > (y1 - y0) else "H"
        for orient in (prefer, "H" if prefer == "V" else "V"):
            lo, hi = (0, 2) if orient == "V" else (1, 3)
            order = sorted(group, key=lambda it: (it[1][lo], it[1][hi]))
            centre = ((x0 + x1) if orient == "V" else (y0 + y1)) / 2
            best = None
            for i in range(1, len(order)):
                left, right = order[:i], order[i:]
                end = max(r[hi] for _, r in left)
                start = min(r[lo] for _, r in right)
                if end <= start + tol:
                    cut = (end + start) / 2
                    score = abs(cut - centre)
                    if best is None or score < best[0]:
                        best = (score, left, right)
            if best:
                a, b = rec(best[1]), rec(best[2])
                if a is None or b is None:
                    return None
                return (orient, a, b)
        return None
    return rec(items) if items else None


def match(sim, saved, mapping):
    """Is the simulated tree the saved tree? Fills mapping saved-key -> insertion index."""
    if sim[0] == "L" and saved[0] == "L":
        mapping[saved[1]] = sim[1]
        return True
    if sim[0] == "L" or saved[0] == "L" or sim[0] != saved[0]:
        return False
    return match(sim[1], saved[1], mapping) and match(sim[2], saved[2], mapping)


def layout_box(items):
    x0 = min(r[0] for _, r in items) - OUTER_GAP
    y0 = min(r[1] for _, r in items) - OUTER_GAP
    x1 = max(r[2] for _, r in items) + OUTER_GAP
    y1 = max(r[3] for _, r in items) + OUTER_GAP
    return (x0, y0, x1, y1)


# ------------------------------------------------------------------ subcommands

def read_clients(path):
    with open(path) as f:
        return json.load(f)


def reading_order(rects):
    return sorted(rects, key=lambda t: (t[2][0], t[2][1], t[0]))


def fmt(argv):
    return " ".join(argv)


def cmd_order(args):
    path, ws, rule = args[0], int(args[1]), args[2]
    pointer = None
    if "--pointer" in args:
        pointer = tuple(float(v) for v in args[args.index("--pointer") + 1].split(","))
    rects = rects_from_clients(read_clients(path), ws)
    items = [(i, r) for i, _, r in rects]
    fallback = reading_order(rects)

    def emit(mode, ordered):
        print(mode)
        for _, argv, r in ordered:
            print(f"{int(r[0])} {int(r[1])} {fmt(argv)}")

    tree = reconstruct(items)
    if tree is None:
        emit("MODE fallback saved rectangles are not a dwindle partition", fallback)
        return 3
    box = layout_box(items)
    n = len(items)
    sim = simulate(n, rule, [pointer] * (n - 1), box)
    if sim is None:
        emit(f"MODE fallback rule {rule} needs a pointer", fallback)
        return 3
    mapping = {}
    if not match(sim[1], tree, mapping):
        emit(f"MODE fallback tree is unreachable under rule {rule} (pointer {pointer})", fallback)
        return 3
    by_key = {i: (i, argv, r) for i, argv, r in rects}
    ordered = [by_key[k] for k in sorted(mapping, key=lambda key: mapping[key])]
    emit(f"MODE computed rule {rule}", ordered)
    return 0


def cmd_steer(args):
    """Insertion order plus, per step, the box a pointer must be inside so that a pointer-nearest
    rule splits the right leaf. Any dwindle tree is reachable this way."""
    path, ws = args[0], int(args[1])
    rects = rects_from_clients(read_clients(path), ws)
    items = [(i, r) for i, _, r in rects]
    tree = reconstruct(items)
    if tree is None:
        print("MODE fallback saved rectangles are not a dwindle partition")
        for _, argv, r in reading_order(rects):
            print(f"{int(r[0])} {int(r[1])} {fmt(argv)}")
        return 3
    box = layout_box(items)
    # Grow breadth-first: the pending splits are the internal nodes of the saved tree; the
    # occupant of an internal node is the occupant of its first child's first leaf.
    steps = []   # (target occupant key, new window key, target box)
    queue = [(tree, box)]
    def occupant(node):
        while node[0] != "L":
            node = node[1]
        return node[1]
    order = [occupant(tree)]
    while queue:
        node, nbox = queue.pop(0)
        if node[0] == "L":
            continue
        orient, a, b = node
        _, first, second = split_box(nbox)
        steps.append((occupant(a), occupant(b), nbox))
        order.append(occupant(b))
        queue.append((a, first))
        queue.append((b, second))
    by_key = {i: (i, argv, r) for i, argv, r in rects}
    print("MODE steer")
    for key in order:
        print(f"{int(by_key[key][2][0])} {int(by_key[key][2][1])} {fmt(by_key[key][1])}")
    # target boxes, one per insertion k >= 2, in the same order as the lines above
    for _, new, nbox in steps:
        cx, cy = (nbox[0] + nbox[2]) / 2, (nbox[1] + nbox[3]) / 2
        print(f"POINTER {int(cx)} {int(cy)}")
    return 0


def load_table(path):
    table = {}
    with open(path) as f:
        for line in f:
            tag, x, y, w, h = line.rstrip("\n").split("\t")
            table[tag] = (int(x), int(y), int(x) + int(w), int(y) + int(h))
    return table


def cmd_derive(args):
    rundir = args[0]
    with open(os.path.join(rundir, "meta.json")) as f:
        meta = json.load(f)
    order = meta["order"]
    tables = [load_table(os.path.join(rundir, f"t{k}.tsv")) for k in range(1, len(order) + 1)]
    targets = []
    for k in range(1, len(tables)):
        before, after = tables[k - 1], tables[k]
        changed = [t for t in before if t in after and before[t] != after[t]]
        targets.append(order.index(changed[0]) + 1 if len(changed) == 1 else None)
    final = tables[-1]
    run = dict(meta)
    run["targets"] = targets
    run["final"] = {str(order.index(t) + 1): list(r) for t, r in final.items()}
    run["box"] = list(layout_box(list(final.items())))
    with open(os.path.join(os.path.dirname(rundir.rstrip("/")), "rule.jsonl"), "a") as f:
        f.write(json.dumps(run) + "\n")
    print(f"RUN {meta['label']}: n={len(order)} insertion order={','.join(order)} "
          f"split targets T={targets} pointers={meta.get('pointers')}")
    for k, r in sorted(((int(k), r) for k, r in run["final"].items())):
        print(f"     slot {k}: {order[k - 1]} at=({r[0]},{r[1]}) size=({r[2] - r[0]},{r[3] - r[1]})")
    return 0


def cmd_score(args):
    runs = [json.loads(line) for line in open(args[0]) if line.strip()]
    runs = [r for r in runs if not r.get("discounted")]
    print(f"{len(runs)} runs; per hypothesis: steps matched / steps, runs matched fully")
    best = None
    for rule in RULES:
        steps = good = full = 0
        for r in runs:
            n = len(r["order"])
            box = tuple(r.get("box") or (0, 26, 1920, 1080))
            pointers = [tuple(p) if p else None for p in r.get("pointers") or []]
            sim = simulate(n, rule, pointers, box)
            if sim is None:
                continue
            ok = [a == b for a, b in zip(sim[0], r["targets"])]
            steps += len(r["targets"]); good += sum(ok)
            full += all(ok) and len(ok) == len(r["targets"])
        print(f"  {rule:9s} {good:3d}/{steps:<3d}  runs {full}/{len(runs)}")
        if steps and (best is None or (full, good) > best[0]):
            best = ((full, good), rule)
    print(f"BEST {best[1] if best else 'none'}")
    return 0


def cmd_tree(args):
    rects = rects_from_clients(read_clients(args[0]), int(args[1]))
    items = [(i, r) for i, _, r in rects]
    print(json.dumps(reconstruct(items)))
    return 0


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    cmds = {"derive": cmd_derive, "score": cmd_score, "tree": cmd_tree, "order": cmd_order, "steer": cmd_steer}
    if argv[1] not in cmds:
        print(__doc__)
        return 2
    return cmds[argv[1]](argv[2:])


if __name__ == "__main__":
    sys.exit(main(sys.argv))
