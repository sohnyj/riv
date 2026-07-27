// Ordered (Bayer) dither.

#include "ps_shared.hlsl"

float4 main(float4 position : SV_POSITION) : SV_Target
{
    float4 color = scene_texture.Load(int3(position.xy, 0));
    color.rgb = dither_quantize(color.rgb, ordered_bias(position.xy));
    return color;
}
