// Fruit (blue noise) dither.

#include "ps_shared.hlsl"

float4 main(float4 position : SV_POSITION) : SV_Target
{
    float4 color = scene_texture.Load(int3(position.xy, 0));
    color.rgb = dither_quantize(color.rgb, blue_noise_bias(blue_noise_texture, position.xy));
    return color;
}
