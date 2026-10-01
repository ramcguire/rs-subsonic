"""Export CLAP's audio tower for rs-subsonic's `clap` analyzer.

Writes laion/clap-htsat-unfused's audio encoder and projection as ONNX: input
`mel` (batch, 1, 1001, 64) log-mel frames, output `embedding` (batch, 512),
unit length. rs-subsonic computes the log-mel frames itself (src/mel.rs).

    uv run --with torch --with transformers --with onnx --with onnxruntime \
        python export_clap.py clap_audio.onnx

Then set `analysis.clap_model` to the written file (about 120 MB).
"""

import sys

import numpy as np
import torch
from transformers import ClapModel

MODEL = "laion/clap-htsat-unfused"


class AudioTower(torch.nn.Module):
    def __init__(self, model):
        super().__init__()
        self.model = model

    def forward(self, mel):
        e = self.model.get_audio_features(input_features=mel)
        # transformers 5 wraps the projection in an output object.
        e = getattr(e, "pooler_output", e)
        return e / e.norm(dim=-1, keepdim=True)


def main(out):
    tower = AudioTower(ClapModel.from_pretrained(MODEL).eval()).eval()
    x = torch.randn(1, 1, 1001, 64)
    with torch.no_grad():
        ref = tower(x).numpy()
    torch.onnx.export(
        tower,
        (x,),
        out,
        input_names=["mel"],
        output_names=["embedding"],
        opset_version=17,
        dynamic_axes={"mel": {0: "batch"}, "embedding": {0: "batch"}},
        dynamo=False,
    )
    try:
        import onnxruntime

        got = onnxruntime.InferenceSession(out).run(None, {"mel": x.numpy()})[0]
        print(f"wrote {out}; max difference from torch {np.abs(got - ref).max():.2e}")
    except ImportError:
        print(f"wrote {out} (install onnxruntime to check it)")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "clap_audio.onnx")
