"""逐像素比对：控制端存出的帧 vs 真机自己的截图（M2 那套方法）。

两张都是 PPM(P6)。为了不受 H.264 有损压缩的细节差异影响，按 M2 的做法
先缩到 480x270（盒式平均），再算相关系数与平均绝对误差。
"""
import sys


def read_ppm(path):
    with open(path, "rb") as f:
        assert f.readline().strip() == b"P6", path
        line = f.readline().split()
        while len(line) < 2:          # 注释行
            line += f.readline().split()
        w, h = int(line[0]), int(line[1])
        mx = int(f.readline())
        assert mx == 255, mx
        data = f.read()
    assert len(data) == w * h * 3, (path, w, h, len(data))
    return w, h, data


def to_gray_small(w, h, data, tw, th):
    out = []
    for ty in range(th):
        y0 = ty * h // th
        y1 = max(y0 + 1, (ty + 1) * h // th)
        for tx in range(tw):
            x0 = tx * w // tw
            x1 = max(x0 + 1, (tx + 1) * w // tw)
            s = 0
            n = 0
            for y in range(y0, y1):
                base = y * w * 3
                for x in range(x0, x1):
                    i = base + x * 3
                    s += data[i] * 299 + data[i + 1] * 587 + data[i + 2] * 114
                    n += 1
            out.append(s / (1000.0 * n))     # 0..255 灰度
    return out


def main(a, b):
    wa, ha, da = read_ppm(a)
    wb, hb, db = read_ppm(b)
    print(f"A={a} {wa}x{ha}   B={b} {wb}x{hb}")
    if (wa, ha) != (wb, hb):
        print("尺寸不同 —— 无法逐像素比对")
        return 2
    ta, tb = to_gray_small(wa, ha, da, 480, 270), to_gray_small(wb, hb, db, 480, 270)
    n = len(ta)
    ma, mb = sum(ta) / n, sum(tb) / n
    cov = sum((ta[i] - ma) * (tb[i] - mb) for i in range(n))
    va = sum((x - ma) ** 2 for x in ta)
    vb = sum((x - mb) ** 2 for x in tb)
    corr = cov / ((va ** 0.5) * (vb ** 0.5)) if va and vb else 0.0
    mae = sum(abs(ta[i] - tb[i]) for i in range(n)) / n
    print(f"相关系数   : {corr:.4f}")
    print(f"平均绝对误差: {mae:.2f}/255 = {mae / 2.55:.2f}%")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1], sys.argv[2]))