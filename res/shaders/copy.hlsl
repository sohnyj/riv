// No dither: the backbuffer write alone quantizes.

float4 main(float4 position : SV_POSITION) : SV_Target
{
    return scene_texture.Load(int3(position.xy, 0));
}
