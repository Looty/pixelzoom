Texture2D<float4> tex : register(t0);
SamplerState lin : register(s0);

cbuffer CB : register(b0)
{
    float2 cursor; // physical px
    float2 size;   // desktop size, px
    float zoom;
    uint mode;     // 0 nearest, 1 bilinear, 2 catmull-rom, 3 lanczos3, 4 sharp-bilinear
    float sharpen; // strength
    uint smode;    // 0 off, 1 unsharp, 2 adaptive edge snap
    uint flags;    // bit 0: panel sub-pixels run B,G,R instead of R,G,B; bit 1: text clean-up off; bit 2: HDR (linear scRGB)
};

struct VSOut { float4 pos : SV_Position; };

VSOut vs_main(uint id : SV_VertexID)
{
    VSOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}

float4 catmull_rom(float2 r)
{
    float2 center = floor(r - 0.5) + 0.5;
    float2 f = r - center;
    float2 f2 = f * f;
    float2 f3 = f2 * f;
    float2 w0 = -0.5 * f3 + f2 - 0.5 * f;
    float2 w1 = 1.5 * f3 - 2.5 * f2 + 1.0;
    float2 w2 = -1.5 * f3 + 2.0 * f2 + 0.5 * f;
    float2 w3 = 0.5 * f3 - 0.5 * f2;
    float2 w12 = w1 + w2;
    float2 t0 = (center - 1.0) / size;
    float2 t3 = (center + 2.0) / size;
    float2 t12 = (center + w2 / w12) / size;

    float4 c = 0;
    c += tex.SampleLevel(lin, float2(t0.x, t0.y), 0) * w0.x * w0.y;
    c += tex.SampleLevel(lin, float2(t12.x, t0.y), 0) * w12.x * w0.y;
    c += tex.SampleLevel(lin, float2(t3.x, t0.y), 0) * w3.x * w0.y;
    c += tex.SampleLevel(lin, float2(t0.x, t12.y), 0) * w0.x * w12.y;
    c += tex.SampleLevel(lin, float2(t12.x, t12.y), 0) * w12.x * w12.y;
    c += tex.SampleLevel(lin, float2(t3.x, t12.y), 0) * w3.x * w12.y;
    c += tex.SampleLevel(lin, float2(t0.x, t3.y), 0) * w0.x * w3.y;
    c += tex.SampleLevel(lin, float2(t12.x, t3.y), 0) * w12.x * w3.y;
    c += tex.SampleLevel(lin, float2(t3.x, t3.y), 0) * w3.x * w3.y;
    return c;
}

float lanczos(float x)
{
    x = abs(x);
    if (x < 1e-4) return 1.0;
    if (x >= 3.0) return 0.0;
    float px = 3.14159265 * x;
    return 3.0 * sin(px) * sin(px / 3.0) / (px * px);
}

float4 lanczos3(float2 r)
{
    float2 p = r - 0.5;
    float2 base = floor(p);
    int2 maxp = int2(size) - 1;
    float4 sum = 0;
    float wsum = 0;
    [unroll] for (int j = -2; j <= 3; j++)
    {
        float wy = lanczos(p.y - (base.y + j));
        [unroll] for (int i = -2; i <= 3; i++)
        {
            float w = lanczos(p.x - (base.x + i)) * wy;
            int2 ip = clamp(int2(base) + int2(i, j), int2(0, 0), maxp);
            sum += tex.Load(int3(ip, 0)) * w;
            wsum += w;
        }
    }
    return sum / wsum;
}

// Each source pixel stays a crisp block; blending happens only across a
// ~1 screen-pixel ramp at block edges.
float4 sharp_bilinear(float2 r)
{
    float2 fl = floor(r);
    float2 t = fl + 0.5 + clamp((r - fl - 0.5) * zoom, -0.5, 0.5);
    return tex.SampleLevel(lin, t / size, 0);
}

float4 filter_at(float2 r)
{
    if (mode == 0)
        return tex.Load(int3(clamp(int2(floor(r)), int2(0, 0), int2(size) - 1), 0));
    if (mode == 1)
        return tex.SampleLevel(lin, r / size, 0);
    if (mode == 2)
        return catmull_rom(r);
    if (mode == 3)
        return lanczos3(r);
    return sharp_bilinear(r);
}

// One sub-pixel sample: k indexes R,G,B,R,G,B... across a row (B,G,R,... on a BGR panel).
float sub_at(int k, int y)
{
    int2 sz = int2(size);
    k = clamp(k, 0, 3 * sz.x - 1);
    int px = k / 3;
    int ch = k - px * 3;
    if (flags & 1) ch = 2 - ch;
    float3 c = tex.Load(int3(px, clamp(y, 0, sz.y - 1), 0)).rgb;
    return ch == 0 ? c.r : (ch == 1 ? c.g : c.b);
}

float4 catw(float f)
{
    float f2 = f * f, f3 = f2 * f;
    return float4(-0.5 * f3 + f2 - 0.5 * f,
                  1.5 * f3 - 2.5 * f2 + 1.0,
                  -1.5 * f3 + 2.0 * f2 + 0.5 * f,
                  0.5 * f3 - 0.5 * f2);
}

// ClearType text stores ~3x horizontal detail in its R,G,B sub-pixels. Treat the
// sub-pixels as a 3x-wide grayscale image and reconstruct it (Catmull-Rom).
float cleartype_gray(float2 r)
{
    float u = r.x * 3.0 - 0.5;
    int kb = (int)floor(u);
    float4 wx = catw(u - kb);
    float v = r.y - 0.5;
    int yb = (int)floor(v);
    float4 wy = catw(v - yb);
    float sum = 0;
    [unroll] for (int j = 0; j < 4; j++)
    {
        float row = 0;
        [unroll] for (int i3 = 0; i3 < 4; i3++)
            row += wx[i3] * sub_at(kb - 1 + i3, yb - 1 + j);
        sum += wy[j] * row;
    }
    return sum;
}

float luma(float3 c)
{
    return dot(c, float3(0.2126, 0.7152, 0.0722));
}

struct Nb
{
    float lmn, lmx;   // luma min/max over the 3x3 around the pixel (output values)
    float3 cmn, cmx;  // the colours at that min and max
    float emn, emx;   // the same min/max, display-encoded (see enc), for the edge tests
    float ewrange;    // encoded luma range over the wider 7x3 window
    float neutral;    // 1 where the colour is only ClearType fringing on grey text
};

float chroma(float3 c)
{
    float3 d = abs(c - luma(c));
    return max(d.r, max(d.g, d.b));
}

// The edge and fringe tests are tuned on display-encoded values (what an SDR
// capture holds, and where ClearType's fringes cancel). An HDR capture is
// linear scRGB (1.0 = 80 nits), so encode it first, roughly (gamma 2.2).
// Only the decisions use this; the drawn colours stay as captured.
float3 enc(float3 c)
{
    return (flags & 4) ? pow(max(c, 0.0), 1.0 / 2.2) : c;
}

// Neighbourhood of source pixel p, and whether its colour is only ClearType
// fringing on grey text (per-pixel saturation can't tell: a thin black stem's
// darkest pixel is itself strongly tinted). ClearType's filter removes the
// colour pattern its R,G,B sub-pixels would add, so across a stroke the
// fringes cancel and a 7x3 window's mean colour stays near grey. Tests, all
// must pass:
// - the mean's tint is small next to the 3x3 contrast, and next to the
//   pixels' own tints (they cancel rather than share a hue);
// - the background (the window extreme nearer the mean brightness, as paper
//   covers most of a text window) is grey: text on a coloured highlight or
//   banner keeps its colour;
// - the summed ink, (pixel - background) over the window, isn't clearly
//   coloured: rejects chart lines and coloured text, muted syntax colours
//   included. The sum of a cut-off stroke's fringes is only roughly grey, so
//   this limit trades fringe removal on grey text (looser) against keeping
//   muted colours (tighter). At 0.12-0.22, VS Code's syntax colours in code
//   fonts keep almost all their colour;
// - the pixel differs from its left or right neighbour: flat colour isn't a
//   fringe.
// Known misses (a local test can't tell): muted colours in thin UI fonts
// (brown, slate, warm grey) lose part of their tint; nearly grey colours
// (Solarized base01) most of it; two complementary 1 px lines side by side
// (orange|blue) look exactly like a fringed black stroke and go grey; a lone
// 1 px stem whose fringes don't fully cancel keeps them.
Nb neighbourhood(int2 p)
{
    int2 maxp = int2(size) - 1;
    Nb n;
    n.lmn = 1e9; n.lmx = -1e9; n.cmn = 0; n.cmx = 0;
    n.emn = 1e9; n.emx = -1e9;
    // The fringe tests below run on encoded colours (se); lmn/lmx/cmn/cmx keep
    // the captured values for the output.
    float wmn = 1e9, wmx = -1e9;
    float3 wcmn = 0, wcmx = 0, sum = 0, lft = 0, centre = 0, rgt = 0;
    float csum = 0;
    [unroll] for (int j = -1; j <= 1; j++)
    {
        [unroll] for (int i = -3; i <= 3; i++)
        {
            float3 sc = tex.Load(int3(clamp(p + int2(i, j), int2(0, 0), maxp), 0)).rgb;
            float3 se = enc(sc);
            float l = luma(sc);
            float le = luma(se);
            sum += se;
            csum += chroma(se);
            if (le < wmn) { wmn = le; wcmn = se; }
            if (le > wmx) { wmx = le; wcmx = se; }
            if (abs(i) <= 1)
            {
                if (l < n.lmn) { n.lmn = l; n.cmn = sc; }
                if (l > n.lmx) { n.lmx = l; n.cmx = sc; }
                n.emn = min(n.emn, le);
                n.emx = max(n.emx, le);
            }
            if (j == 0 && i == -1) lft = se;
            if (j == 0 && i == 0) centre = se;
            if (j == 0 && i == 1) rgt = se;
        }
    }
    float3 side = max(abs(lft - centre), abs(rgt - centre));
    n.ewrange = wmx - wmn;
    float ml = luma(sum / 21.0);
    float3 bg = (wmx - ml < ml - wmn) ? wcmx : wcmn;
    float3 ink = sum - 21.0 * bg;
    float tint = chroma(sum / 21.0);
    n.neutral = (1.0 - smoothstep(0.07, 0.13, tint / max(n.emx - n.emn, 0.02)))
              * (1.0 - smoothstep(0.35, 0.55, tint / (csum / 21.0 + 0.02)))
              * (1.0 - smoothstep(0.04, 0.08, chroma(bg)))
              * (1.0 - smoothstep(0.12, 0.22, chroma(ink) / (abs(luma(ink)) + 0.1)))
              * smoothstep(0.01, 0.03, max(side.r, max(side.g, side.b)));
    return n;
}

// Whether the 3x3 around p has enough relative contrast to count as an edge.
// The fractional path's weights are smoothstep(0.12, 0.45, contrast), which
// is zero otherwise, so it skips the full neighbourhood test there.
bool is_edge(int2 p)
{
    int2 maxp = int2(size) - 1;
    float mn = 1e9, mx = -1e9;
    [unroll] for (int j = -1; j <= 1; j++)
    {
        [unroll] for (int i = -1; i <= 1; i++)
        {
            float l = luma(enc(tex.Load(int3(clamp(p + int2(i, j), int2(0, 0), maxp), 0)).rgb));
            mn = min(mn, l);
            mx = max(mx, l);
        }
    }
    return (mx - mn) / (mx + 0.02) > 0.12;
}

float sat3(float3 c)
{
    return (max(c.r, max(c.g, c.b)) - min(c.r, min(c.g, c.b))) / (max(c.r, max(c.g, c.b)) + 0.02);
}

// The rendering from before the text clean-up (v0.2.1), used when "Clean up
// text colour fringes" is off in Settings, so turning it off gives exactly the
// old look rather than a third variant.
float4 classic(float2 pos)
{
    float2 r = cursor + (pos - cursor) / zoom;
    float whole = round(zoom);
    if (whole >= 2.0 && abs(zoom - whole) < 0.005)
    {
        int2 maxp = int2(size) - 1;
        int2 p = clamp(int2(floor(r)), int2(0, 0), maxp);
        float3 blk = tex.Load(int3(p, 0)).rgb;
        if (fmod(whole, 3.0) < 0.5)
        {
            // Multiples of 3: one sub-pixel per output column, shown as grey on
            // neutral (text-like) neighbourhoods.
            float lmn = 1e9, lmx = -1e9;
            float3 cmn = 0, cmx = 0;
            [unroll] for (int j = -1; j <= 1; j++)
            {
                [unroll] for (int i2 = -1; i2 <= 1; i2++)
                {
                    float3 sc = tex.Load(int3(clamp(p + int2(i2, j), int2(0, 0), maxp), 0)).rgb;
                    float l = luma(sc);
                    if (l < lmn) { lmn = l; cmn = sc; }
                    if (l > lmx) { lmx = l; cmx = sc; }
                }
            }
            float spread = max(max(cmn.r, max(cmn.g, cmn.b)) - min(cmn.r, min(cmn.g, cmn.b)),
                               max(cmx.r, max(cmx.g, cmx.b)) - min(cmx.r, min(cmx.g, cmx.b)));
            float neutral = 1.0 - smoothstep(0.12, 0.30, spread);
            neutral *= smoothstep(0.30, 0.60, (lmx - lmn) / (lmx + 0.02));
            blk += (sub_at((int)floor(r.x * 3.0), p.y) - luma(blk)) * neutral;
        }
        return float4(max(blk, 0.0), 1.0);
    }
    float4 c = filter_at(r);
    if (smode >= 2)
    {
        int2 p = int2(floor(r));
        int2 maxp = int2(size) - 1;
        float lmn = 1e9, lmx = -1e9;
        float3 cmn = 0, cmx = 0;
        [unroll] for (int j = -1; j <= 1; j++)
        {
            [unroll] for (int i2 = -1; i2 <= 1; i2++)
            {
                float3 sc = tex.Load(int3(clamp(p + int2(i2, j), int2(0, 0), maxp), 0)).rgb;
                float l = luma(sc);
                if (l < lmn) { lmn = l; cmn = sc; }
                if (l > lmx) { lmx = l; cmx = sc; }
            }
        }
        float range = lmx - lmn;
        float conf = smoothstep(0.12, 0.45, range / (lmx + 0.02));
        if (smode >= 3)
        {
            float g = clamp(cleartype_gray(r), lmn, lmx);
            float neutral = (1.0 - smoothstep(0.08, 0.25, max(sat3(cmn), sat3(cmx)))) * conf;
            c.rgb += (g - luma(c.rgb)) * neutral;
        }
        if (smode != 3)
        {
            float t = saturate((luma(c.rgb) - lmn) / max(range, 1e-4));
            float t2 = saturate((t - 0.5) * (1.0 + sharpen * zoom * 1.5) + 0.5);
            c.rgb = lerp(c.rgb, lerp(cmn, cmx, t2), conf);
        }
    }
    else if (smode == 1)
    {
        float2 d = 1.0 / zoom;
        float4 n = tex.SampleLevel(lin, (r + float2(0, -d.y)) / size, 0);
        float4 s = tex.SampleLevel(lin, (r + float2(0, d.y)) / size, 0);
        float4 e = tex.SampleLevel(lin, (r + float2(d.x, 0)) / size, 0);
        float4 w = tex.SampleLevel(lin, (r + float2(-d.x, 0)) / size, 0);
        float4 mn = min(c, min(min(n, s), min(e, w)));
        float4 mx = max(c, max(max(n, s), max(e, w)));
        c = clamp(c + sharpen * (c - (n + s + e + w) * 0.25), mn, mx);
    }
    return float4(max(c.rgb, 0.0), 1.0);
}

float4 ps_main(VSOut i) : SV_Target
{
    if (flags & 2)
        return classic(i.pos.xy);
    // Proportional-follow transform: the real cursor always sits over the
    // content it would click, so no input remapping is needed.
    float2 r = cursor + (i.pos.xy - cursor) / zoom;
    // Whole-number zoom: the cursor is an integer pixel, so every source pixel
    // maps to an exact N x N block (positions use N itself, so the blocks stay
    // exact while the zoom eases within the snap tolerance).
    float whole = round(zoom);
    if (whole >= 2.0 && abs(zoom - whole) < 0.005)
    {
        float2 rw = cursor + (i.pos.xy - cursor) / whole;
        int2 maxp = int2(size) - 1;
        int2 p = clamp(int2(floor(rw)), int2(0, 0), maxp);
        float3 blk = tex.Load(int3(p, 0)).rgb;
        // ClearType text holds ~3x horizontal detail in its sub-pixels. Each
        // output column covers 3/N sub-pixels of its row (exactly one at 3x and
        // 6x, 1.5 at 2x, 0.75 at 4x): show their box average as grey, so text
        // gets the true sub-pixel detail instead of orange/blue blocks. Rows
        // stay exact blocks. Only on ClearType-on-grey text edges, and only in
        // the ClearType-aware sharpen modes (3, 4); everything else, coloured
        // content included, stays the exact captured blocks. A grey pixel's
        // sub-pixels are equal and each column's span stays inside its source
        // pixel, so it would come out unchanged: skip the test there.
        if (smode < 3 || chroma(enc(blk)) < 0.004)
            return float4(max(blk, 0.0), 1.0);
        Nb n = neighbourhood(p);
        // Only at real edges: low-contrast areas keep the block.
        float w = n.neutral * smoothstep(0.12, 0.45, (n.emx - n.emn) / (n.emx + 0.02));
        if (w > 0.0)
        {
            float a = rw.x * 3.0 - 1.5 / whole;
            float b = rw.x * 3.0 + 1.5 / whole;
            int k0 = (int)floor(a);
            float s = 0;
            [unroll] for (int k = 0; k < 3; k++)
            {
                float lo = max(a, (float)(k0 + k));
                float hi = min(b, (float)(k0 + k + 1));
                s += max(hi - lo, 0.0) * sub_at(k0 + k, p.y);
            }
            blk = lerp(blk, (s / (b - a)).xxx, w);
        }
        return float4(max(blk, 0.0), 1.0);
    }
    float4 c = filter_at(r);
    // Below this 3x3 contrast both the text reconstruction and the snap have
    // zero weight (conf below), so flat areas, most of a screen, skip the
    // wider neighbourhood test with identical output.
    if (smode >= 2 && is_edge(int2(floor(r))))
    {
        // Edge-adaptive snapping: steepen each edge towards the local min/max,
        // weighted by local contrast. Flat areas and soft gradients are left
        // alone; output never leaves the local [min, max] range.
        Nb n = neighbourhood(int2(floor(r)));
        float range = n.lmx - n.lmn;
        float erange = n.emx - n.emn;
        float conf = smoothstep(0.12, 0.45, erange / (n.emx + 0.02));
        // ClearType-aware modes: grey text edges, weighted by the edge test.
        float text = smode >= 3 ? n.neutral : 0.0;
        if (smode >= 3)
        {
            // ClearType-on-grey text edges get the sub-pixel grayscale
            // reconstruction in place of the filtered colour, which removes the
            // orange/blue fringes; coloured text and photos keep the filter.
            float g = clamp(cleartype_gray(r), n.lmn, n.lmx);
            c.rgb = lerp(c.rgb, g.xxx, text * conf);
        }
        if (smode != 3)
        {
            // Only sharp edges: a soft ramp spans more than the 3x3 window, so
            // its 3x3 range is a small part of the 7x3 range. And an absolute
            // floor, so faint steps in shadows and dark UI aren't snapped.
            float sharp = smoothstep(0.55, 0.85, erange / max(n.ewrange, 1e-4)) * smoothstep(0.06, 0.15, erange);
            float cl = luma(c.rgb);
            float t = saturate((cl - n.lmn) / max(range, 1e-4));
            float gain = 1.0 + sharpen * zoom * 1.5;
            float t2 = saturate((t - 0.5) * gain + 0.5);
            // Grey text, once reconstructed, snaps in brightness only (snapping
            // towards the tinted min/max pixels would put the fringes back);
            // everything else snaps towards its own min/max colours.
            float3 snapped = lerp(lerp(n.cmn, n.cmx, t2), c.rgb + (lerp(n.lmn, n.lmx, t2) - cl), text);
            c.rgb = lerp(c.rgb, snapped, conf * sharp);
        }
    }
    else if (smode == 1)
    {
        // Unsharp mask on the output grid, clamped to the neighbourhood range
        // so it can't ring or overshoot (HDR-safe).
        float2 d = 1.0 / zoom;
        float4 n = tex.SampleLevel(lin, (r + float2(0, -d.y)) / size, 0);
        float4 s = tex.SampleLevel(lin, (r + float2(0, d.y)) / size, 0);
        float4 e = tex.SampleLevel(lin, (r + float2(d.x, 0)) / size, 0);
        float4 w = tex.SampleLevel(lin, (r + float2(-d.x, 0)) / size, 0);
        float4 mn = min(c, min(min(n, s), min(e, w)));
        float4 mx = max(c, max(max(n, s), max(e, w)));
        float4 blur = (n + s + e + w) * 0.25;
        c = clamp(c + sharpen * (c - blur), mn, mx);
    }
    // No upper clamp: in HDR (scRGB) values above 1.0 are real highlights.
    return float4(max(c.rgb, 0.0), 1.0);
}
