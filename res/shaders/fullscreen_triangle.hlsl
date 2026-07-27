// Fullscreen triangle from SV_VertexID for the quantize pass.

float4 main(uint vertex_id : SV_VertexID) : SV_POSITION
{
    float2 position = float2((vertex_id << 1) & 2, vertex_id & 2);
    return float4(position * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
}
