import sys
import numpy as np
from PIL import Image

path = sys.argv[1]
arr = np.array(Image.open(path).convert("RGB"))
colors = len(np.unique(arr.reshape(-1, 3), axis=0))
std = arr.std()
print("size:", arr.shape, "std:", round(float(std), 2), "colors:", colors)
print("VALID" if std > 10 and colors > 5000 else "CORRUPTED")
