// Ultra HDR gain application: base x 2^(boost x W) into a linear FP16 target.
//
// The base texel is sRGB-encoded BGRA; the gain map holds the encoded recovery
// (ISO 21496-1 lineage). The SDR white boost is folded in so the output is
// absolute scene-referred linear (1.0 = 80 nits), like every other FP16 source.

Texture2D base_texture : register(t0);
Texture2D gain_texture : register(t1);
SamplerState gain_sampler : register(s0);

cbuffer GainConstants : register(b0)
{
    // x = weight W, y = SDR white boost (SdrWhiteLevel / 80 nits).
    float4 weight_and_boost;
    float4 map_gamma;
    float4 boost_minimum;
    float4 boost_maximum;
    float4 offset_sdr;
    float4 offset_hdr;
};

float3 srgb_to_linear(float3 encoded)
{
    float3 linear_segment = encoded / 12.92;
    float3 curved_segment = pow((encoded + 0.055) / 1.055, 2.4);
    return encoded <= 0.04045 ? linear_segment : curved_segment;
}

float4 main(float4 position : SV_POSITION) : SV_Target
{
    float2 base_size;
    base_texture.GetDimensions(base_size.x, base_size.y);
    float3 base = srgb_to_linear(base_texture.Load(int3(position.xy, 0)).rgb);
    // The gain map is usually smaller than the base; sample it bilinearly.
    float3 recovery = gain_texture.SampleLevel(gain_sampler, position.xy / base_size, 0.0).rgb;
    float3 log_recovery = pow(recovery, 1.0 / map_gamma.rgb);
    float3 log_boost = lerp(boost_minimum.rgb, boost_maximum.rgb, log_recovery);
    float3 gained = (base + offset_sdr.rgb) * exp2(log_boost * weight_and_boost.x)
        - offset_hdr.rgb;
    return float4(max(gained, 0.0) * weight_and_boost.y, 1.0);
}
