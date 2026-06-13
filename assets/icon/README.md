# stepper icon

`stepper.svg` is the master (300×300 viewBox: black field, white two-step staircase
on a uniform 3×3 grid — each cell 1/3, edge-to-edge, no margin). Everything else is
generated from it — edit the SVG, then regenerate.

## Outputs
- `stepper-{16,32,48,64,128,256,512,1024}.png` — raster sizes
- `stepper.ico` / `favicon.ico` — multi-resolution (16·32·48·64·128·256), PNG-packed
- `stepper.icns` — macOS icon

## Regenerate
Needs `rsvg-convert` (librsvg), ImageMagick, Python 3 (Pillow), and macOS `iconutil`.

```sh
cd assets/icon
for s in 16 32 48 64 128 256 512 1024; do rsvg-convert -w $s -h $s stepper.svg -o stepper-$s.png; done

# ICO: hand-pack the native PNGs (lean + crisp; ImageMagick bloats the 256 entry)
python3 - <<'PY'
import struct
sizes=[16,32,48,64,128,256]
pngs=[(s, open(f"stepper-{s}.png","rb").read()) for s in sizes]
off=6+16*len(pngs); ent=b''; data=b''
for s,p in pngs:
    d=0 if s>=256 else s
    ent+=struct.pack('<BBBBHHII', d,d,0,0,1,32,len(p),off); off+=len(p); data+=p
open("stepper.ico","wb").write(struct.pack('<HHH',0,1,len(pngs))+ent+data)
PY
cp stepper.ico favicon.ico

# ICNS (macOS)
rm -rf stepper.iconset && mkdir stepper.iconset
cp stepper-16.png stepper.iconset/icon_16x16.png;     cp stepper-32.png  stepper.iconset/icon_16x16@2x.png
cp stepper-32.png stepper.iconset/icon_32x32.png;     cp stepper-64.png  stepper.iconset/icon_32x32@2x.png
cp stepper-128.png stepper.iconset/icon_128x128.png;  cp stepper-256.png stepper.iconset/icon_128x128@2x.png
cp stepper-256.png stepper.iconset/icon_256x256.png;  cp stepper-512.png stepper.iconset/icon_256x256@2x.png
cp stepper-512.png stepper.iconset/icon_512x512.png;  cp stepper-1024.png stepper.iconset/icon_512x512@2x.png
iconutil -c icns stepper.iconset -o stepper.icns && rm -rf stepper.iconset
```
