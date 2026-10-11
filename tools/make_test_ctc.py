"""Build ot-test-ctc.tar.bz2: a synthetic sherpa-onnx CTC model (`--tdnn-model`) that spells
"hello world" for any audio longer than ~0.3 s, for the local engine's end-to-end test
(`stt::local::tests::local_engine_end_to_end`). It proves model loading, feature framing, CTC
decoding and the transport without a real (large) model.

    pip install onnx numpy
    python3 tools/make_test_ctc.py <mirror folder>
"""
import os, sys, tarfile, tempfile
import numpy as np, onnx
from onnx import helper, TensorProto, numpy_helper
text = "hello world"
mirror = sys.argv[1] if len(sys.argv) > 1 else "."
out_dir = os.path.join(tempfile.mkdtemp(), "ot-test-ctc")
os.makedirs(out_dir)
symbols = ["<blk>"] + sorted(set(c for c in text if c != " ")) + ["▁"]
ids = {s: i for i, s in enumerate(symbols)}
seq = []
for c in text:
    seq += [ids["▁" if c == " " else c], 0]  # token then blank, so repeats survive CTC collapse
MAXT = 4000
V = len(symbols)
pat = np.full((1, MAXT, V), -20.0, dtype=np.float32)
pat[0, :, 0] = 0.0  # blank everywhere
for t, tok in enumerate(seq):
    pat[0, t, :] = -20.0
    pat[0, t, tok] = 0.0
x = helper.make_tensor_value_info("x", TensorProto.FLOAT, ["N", "T", "F"])
y = helper.make_tensor_value_info("y", TensorProto.FLOAT, [1, "T", V])
nodes = [
    helper.make_node("Shape", ["x"], ["shape"]),
    helper.make_node("Gather", ["shape", "one"], ["t"], axis=0),
    helper.make_node("Unsqueeze", ["t", "zero"], ["ends"]),
    helper.make_node("Slice", ["pattern", "starts", "ends", "axes"], ["y"]),
]
inits = [
    numpy_helper.from_array(pat, "pattern"),
    numpy_helper.from_array(np.array(1, dtype=np.int64), "one"),
    numpy_helper.from_array(np.array([0], dtype=np.int64), "zero"),
    numpy_helper.from_array(np.array([0], dtype=np.int64), "starts"),
    numpy_helper.from_array(np.array([1], dtype=np.int64), "axes"),
]
g = helper.make_graph(nodes, "fake_ctc", [x], [y], inits)
m = helper.make_model(g, opset_imports=[helper.make_opsetid("", 13)])
m.ir_version = 8
helper.set_model_props(m, {"vocab_size": str(V)})
onnx.checker.check_model(m)
onnx.save(m, f"{out_dir}/model.onnx")
with open(f"{out_dir}/tokens.txt", "w", encoding="utf-8") as f:
    for s, i in ids.items():
        f.write(f"{s} {i}\n")
archive = os.path.join(mirror, "ot-test-ctc.tar.bz2")
with tarfile.open(archive, "w:bz2") as t:
    t.add(out_dir, arcname="ot-test-ctc")
print(archive)
