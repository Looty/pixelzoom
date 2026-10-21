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
    uint flags;    // bit 0: panel sub-pixels run B,G,R instead of R,G,B
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

float sat3(float3 c)
{
    return (max(c.r, max(c.g, c.b)) - min(c.r, min(c.g, c.b))) / (max(c.r, max(c.g, c.b)) + 0.02);
}

float4 ps_main(VSOut i) : SV_Target
{
    // Proportional-follow transform: the real cursor always sits over the
    // content it would click, so no input remapping is needed.
    float2 r = cursor + (i.pos.xy - cursor) / zoom;
    // Whole-number zoom: the cursor is an integer pixel, so every source pixel
    // maps to an exact N x N block. Draw the blocks as they are, no filtering.
    float whole = round(zoom);
    if (whole >= 2.0 && abs(zoom - whole) < 0.005)
    {
        int2 maxp = int2(size) - 1;
        int2 p = clamp(int2(floor(r)), int2(0, 0), maxp);
        float3 blk = tex.Load(int3(p, 0)).rgb;
        if (fmod(whole, 3.0) < 0.5)
        {
            // Multiples of 3: each output column lines up with exactly one
            // sub-pixel (R, G or B) of a source pixel, so ClearType text is
            // shown at its true 3x horizontal resolution, as grey. Only where
            // the neighbourhood is neutral (text-like); coloured content keeps
            // its colour blocks.
            float lmn = 1e9, lmx = -1e9;
            float3 cmn = 0, cmx = 0;
            [unroll] for (int j = -1; j <= 1; j++)
            {
                [unroll] for (int i2 = -1; i2 <= 1; i2++)
                {
                    float3 sc = tex.Load(int3(clamp(p + int2(i2, j), int2(0, 0), maxp), 0)).rgb;
                    float l = dot(sc, float3(0.2126, 0.7152, 0.0722));
                    if (l < lmn) { lmn = l; cmn = sc; }
                    if (l > lmx) { lmx = l; cmx = sc; }
                }
            }
            // Absolute channel spread, not relative saturation: the dark ink pixel
            // of black text is tinted by sub-pixel fringes (high relative
            // saturation) but stays far less spread than real coloured text.
            float spread = max(max(cmn.r, max(cmn.g, cmn.b)) - min(cmn.r, min(cmn.g, cmn.b)),
                               max(cmx.r, max(cmx.g, cmx.b)) - min(cmx.r, min(cmx.g, cmx.b)));
            float neutral = 1.0 - smoothstep(0.12, 0.30, spread);
            // Flat or low-contrast areas aren't text: the per-channel sub-pixel
            // values of a plain colour would show as grey stripes. Keep the block.
            neutral *= smoothstep(0.30, 0.60, (lmx - lmn) / (lmx + 0.02));
            blk += (sub_at((int)floor(r.x * 3.0), p.y) - dot(blk, float3(0.2126, 0.7152, 0.0722))) * neutral;
        }
        return float4(max(blk, 0.0), 1.0);
    }
    float4 c = filter_at(r);
    if (smode >= 2)
    {
        // Edge-adaptive snapping: steepen each edge towards the local min/max
        // colour, weighted by local contrast. Flat areas and soft gradients
        // are left alone; output never leaves the local [min, max] range.
        int2 p = int2(floor(r));
        int2 maxp = int2(size) - 1;
        float lmn = 1e9, lmx = -1e9;
        float3 cmn = 0, cmx = 0;
        [unroll] for (int j = -1; j <= 1; j++)
        {
            [unroll] for (int i2 = -1; i2 <= 1; i2++)
            {
                float3 sc = tex.Load(int3(clamp(p + int2(i2, j), int2(0, 0), maxp), 0)).rgb;
                float l = dot(sc, float3(0.2126, 0.7152, 0.0722));
                if (l < lmn) { lmn = l; cmn = sc; }
                if (l > lmx) { lmx = l; cmx = sc; }
            }
        }
        float range = lmx - lmn;
        float contrast = range / (lmx + 0.02);
        float conf = smoothstep(0.12, 0.45, contrast);
        if (smode >= 3)
        {
            // Neutral (text-like) neighbourhoods get the sub-pixel grayscale
            // reconstruction; coloured content keeps the normal filter.
            float g = clamp(cleartype_gray(r), lmn, lmx);
            float neutral = 1.0 - smoothstep(0.08, 0.25, max(sat3(cmn), sat3(cmx)));
            // Only at real edges (text), and shift brightness rather than replace
            // the colour, so muted photo/video content keeps its hue.
            neutral *= conf;
            float cl0 = dot(c.rgb, float3(0.2126, 0.7152, 0.0722));
            c.rgb += (g - cl0) * neutral;
        }
        if (smode != 3)
        {
            float cl = dot(c.rgb, float3(0.2126, 0.7152, 0.0722));
            float t = saturate((cl - lmn) / max(range, 1e-4));
            float gain = 1.0 + sharpen * zoom * 1.5;
            float t2 = saturate((t - 0.5) * gain + 0.5);
            c.rgb = lerp(c.rgb, lerp(cmn, cmx, t2), conf);
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
