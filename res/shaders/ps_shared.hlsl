// Bindings and dither math every quantize-pass pixel shader is compiled with.
//
// The dither math is ported from libplacebo's pl_shader_dither
// (src/shaders/dithering.c, LGPL-2.1-or-later): ordered-fixed and
// blue-noise bias generation followed by a biased floor quantization.

Texture2D scene_texture : register(t0);
Texture2D blue_noise_texture : register(t1);

cbuffer QuantizationConstants : register(b0)
{
    float quantization_steps;
    float3 padding;
};

float ordered_bias(float2 position)
{
    float2 pos = frac(position * (1.0 / 16.0));
    uint2 xy = uint2(pos * 16.0) % 16u;
    xy.x = xy.x ^ xy.y;
    xy = (xy | xy << 2) & 0x33333333u;
    xy = (xy | xy << 1) & 0x55555555u;
    uint b = xy.x + (xy.y << 1);
    b = (b * 0x0802u & 0x22110u) | (b * 0x8020u & 0x88440u);
    b = 0x10101u * b;
    b = (b >> 16) & 0xFFu;
    return float(b) * (1.0 / 256.0);
}

float blue_noise_bias(Texture2D noise, float2 position)
{
    float2 pos = frac(position * (1.0 / BLUE_NOISE_EDGE_TEXELS));
    return noise.Load(int3(int2(pos * BLUE_NOISE_EDGE_TEXELS), 0)).r;
}

float3 dither_quantize(float3 color, float bias)
{
    const float scale = quantization_steps;
    color = (abs(color) < 1e-5) ? float3(0.0, 0.0, 0.0) : color;
    color = scale * color + bias;
    return floor(color) * (1.0 / scale);
}
