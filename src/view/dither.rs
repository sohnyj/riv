//! Custom D2D output-dither effects: Ordered (Bayer 16x16) and Fruit (blue noise).
//!
//! The dither math is ported from libplacebo's pl_shader_dither
//! (src/shaders/dithering.c, LGPL-2.1-or-later): ordered-fixed and
//! blue-noise bias generation followed by a biased floor quantization.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, RECT, S_OK};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BUFFER_PRECISION_32BPC_FLOAT, D2D1_CHANGE_TYPE, D2D1_CHANNEL_DEPTH_1,
    D2D1_EXTEND_MODE_WRAP, D2D1_FILTER_MIN_MAG_MIP_POINT, D2D1_PIXEL_OPTIONS_NONE,
    D2D1_RESOURCE_TEXTURE_PROPERTIES, ID2D1DrawInfo, ID2D1DrawTransform, ID2D1DrawTransform_Impl,
    ID2D1EffectContext, ID2D1EffectImpl, ID2D1EffectImpl_Impl, ID2D1Factory1, ID2D1ResourceTexture,
    ID2D1Transform_Impl, ID2D1TransformGraph, ID2D1TransformNode_Impl,
};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::core::{
    GUID, HRESULT, IUnknown, IUnknownImpl, Interface, OutRef, PCSTR, Ref, Result, implement, s, w,
};

pub const CLSID_RIV_DITHER_ORDERED: GUID = GUID::from_u128(0x8f6c9c1e_3a54_4d21_9b0e_5a4a1c2d7e91);
pub const CLSID_RIV_DITHER_FRUIT: GUID = GUID::from_u128(0x2b7d4f83_916a_4c05_8b47_d3c9e0f6a512);
const SHADER_ORDERED: GUID = GUID::from_u128(0x64b8e2a1_7c3f_49d6_a0b5_29f1c8d47e63);
const SHADER_FRUIT: GUID = GUID::from_u128(0xd94a3c76_58e2_4b1f_9c08_7a61b5f0d284);

/// The D2D device caches loaded shaders by GUID; each step count is its own shader.
fn shader_identifier(base: GUID, quantization_steps: u32) -> GUID {
    GUID {
        data1: base.data1 ^ quantization_steps,
        ..base
    }
}
const BLUE_NOISE_TEXTURE: GUID = GUID::from_u128(0x417f9d25_c680_4e8a_b1d7_063e94a8c5f2);

const BLUE_NOISE_SIZE: u32 = super::blue_noise::SIZE as u32;

// Runtime-agnostic core shared by both entry points; the bytecode is plain
// shader model 5.0 DXBC, loadable by Direct3D 11 and Direct3D 12 alike.
const SHADER_PROLOGUE: &str = "\
Texture2D input_texture : register(t0);
SamplerState input_sampler : register(s0);

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
    float2 pos = frac(position * (1.0 / 64.0));
    return noise.Load(int3(int2(pos * 64.0), 0)).r;
}

float3 dither_quantize(float3 color, float bias)
{
    const float scale = QUANTIZATION_STEPS;
    color = (abs(color) < 1e-5) ? float3(0.0, 0.0, 0.0) : color;
    color = scale * color + bias;
    return floor(color) * (1.0 / scale);
}
";

const ORDERED_SHADER_SOURCE: &str = "\
float4 main(float4 clip_position : SV_POSITION,
            float4 scene_position : SCENE_POSITION,
            float4 texture_coordinate : TEXCOORD0) : SV_Target
{
    float4 color = input_texture.Sample(input_sampler, texture_coordinate.xy);
    color.rgb = dither_quantize(color.rgb, ordered_bias(scene_position.xy));
    return color;
}
";

const FRUIT_SHADER_SOURCE: &str = "\
Texture2D blue_noise_texture : register(t1);

float4 main(float4 clip_position : SV_POSITION,
            float4 scene_position : SCENE_POSITION,
            float4 texture_coordinate : TEXCOORD0) : SV_Target
{
    float4 color = input_texture.Sample(input_sampler, texture_coordinate.xy);
    color.rgb =
        dither_quantize(color.rgb, blue_noise_bias(blue_noise_texture, scene_position.xy));
    return color;
}
";

pub(crate) fn compile_shader(source: &str, profile: PCSTR) -> Result<Vec<u8>> {
    let mut code = None;
    unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            s!("riv_shader"),
            None,
            None,
            s!("main"),
            profile,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &raw mut code,
            None,
        )?;
    }
    let blob = code.ok_or_else(windows::core::Error::empty)?;
    let bytes = unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    };
    Ok(bytes.to_vec())
}

/// Effects bake the steps of the registration they were created under.
static REGISTERED_QUANTIZATION_STEPS: AtomicU32 = AtomicU32::new(255);

type BytecodeCache = Mutex<HashMap<(u8, u32), &'static [u8]>>;

fn compiled_bytecode(pattern: DitherPattern, quantization_steps: u32) -> Result<&'static [u8]> {
    static CACHE: OnceLock<BytecodeCache> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("dither bytecode cache poisoned");
    let key = (pattern as u8, quantization_steps);
    if let Some(bytecode) = cache.get(&key) {
        return Ok(bytecode);
    }
    let body = match pattern {
        DitherPattern::Ordered => ORDERED_SHADER_SOURCE,
        DitherPattern::Fruit => FRUIT_SHADER_SOURCE,
    };
    let source = format!("{SHADER_PROLOGUE}{body}")
        .replace("QUANTIZATION_STEPS", &format!("{quantization_steps}.0"));
    let bytecode: &'static [u8] =
        Box::leak(compile_shader(&source, s!("ps_5_0"))?.into_boxed_slice());
    cache.insert(key, bytecode);
    Ok(bytecode)
}

/// Single-channel f32 texels, generated once per process.
fn blue_noise_texels() -> &'static [u8] {
    static TEXELS: OnceLock<Vec<u8>> = OnceLock::new();
    TEXELS.get_or_init(|| {
        super::blue_noise::generate()
            .iter()
            .flat_map(|value| value.to_ne_bytes())
            .collect()
    })
}

/// Settings-selected output dither (0 = None, 1 = Ordered, 2 = Fruit).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DitherMode {
    None,
    Ordered,
    Fruit,
}

impl DitherMode {
    pub fn from_setting(value: u32) -> Self {
        match value {
            1 => Self::Ordered,
            2 => Self::Fruit,
            _ => Self::None,
        }
    }
}

#[derive(Clone, Copy)]
enum DitherPattern {
    Ordered,
    Fruit,
}

#[implement(ID2D1EffectImpl, ID2D1DrawTransform)]
struct DitherEffect {
    pattern: DitherPattern,
    quantization_steps: u32,
    blue_noise_texture: RefCell<Option<ID2D1ResourceTexture>>,
}

impl ID2D1EffectImpl_Impl for DitherEffect_Impl {
    fn Initialize(
        &self,
        effectcontext: Ref<'_, ID2D1EffectContext>,
        transformgraph: Ref<'_, ID2D1TransformGraph>,
    ) -> Result<()> {
        let context = effectcontext.ok()?;
        let graph = transformgraph.ok()?;
        match self.pattern {
            DitherPattern::Ordered => unsafe {
                context.LoadPixelShader(
                    &shader_identifier(SHADER_ORDERED, self.quantization_steps),
                    compiled_bytecode(DitherPattern::Ordered, self.quantization_steps)?,
                )?;
            },
            DitherPattern::Fruit => {
                unsafe {
                    context.LoadPixelShader(
                        &shader_identifier(SHADER_FRUIT, self.quantization_steps),
                        compiled_bytecode(DitherPattern::Fruit, self.quantization_steps)?,
                    )
                }?;
                // The texture is keyed by GUID and shared across effect instances.
                let texture =
                    unsafe { context.FindResourceTexture(&BLUE_NOISE_TEXTURE) }.or_else(|_| {
                        let extents = [BLUE_NOISE_SIZE, BLUE_NOISE_SIZE];
                        let extend_modes = [D2D1_EXTEND_MODE_WRAP, D2D1_EXTEND_MODE_WRAP];
                        let properties = D2D1_RESOURCE_TEXTURE_PROPERTIES {
                            extents: extents.as_ptr(),
                            dimensions: 2,
                            bufferPrecision: D2D1_BUFFER_PRECISION_32BPC_FLOAT,
                            channelDepth: D2D1_CHANNEL_DEPTH_1,
                            filter: D2D1_FILTER_MIN_MAG_MIP_POINT,
                            extendModes: extend_modes.as_ptr(),
                        };
                        let strides = [BLUE_NOISE_SIZE * 4];
                        unsafe {
                            context.CreateResourceTexture(
                                Some(&BLUE_NOISE_TEXTURE),
                                &raw const properties,
                                Some(blue_noise_texels()),
                                Some(strides.as_ptr()),
                            )
                        }
                    })?;
                *self.blue_noise_texture.borrow_mut() = Some(texture);
            }
        }
        let node: ID2D1DrawTransform = self.to_interface();
        unsafe { graph.SetSingleTransformNode(&node) }
    }

    fn PrepareForRender(&self, _changetype: D2D1_CHANGE_TYPE) -> Result<()> {
        Ok(())
    }

    fn SetGraph(&self, _transformgraph: Ref<'_, ID2D1TransformGraph>) -> Result<()> {
        // Only reached by variable-input effects.
        Err(E_NOTIMPL.into())
    }
}

impl ID2D1TransformNode_Impl for DitherEffect_Impl {
    fn GetInputCount(&self) -> u32 {
        1
    }
}

impl ID2D1Transform_Impl for DitherEffect_Impl {
    fn MapOutputRectToInputRects(
        &self,
        outputrect: *const RECT,
        inputrects: *mut RECT,
        inputrectscount: u32,
    ) -> Result<()> {
        if inputrectscount != 1 {
            return Err(E_INVALIDARG.into());
        }
        unsafe { *inputrects = *outputrect };
        Ok(())
    }

    fn MapInputRectsToOutputRect(
        &self,
        inputrects: *const RECT,
        inputopaquesubrects: *const RECT,
        inputrectcount: u32,
        outputrect: *mut RECT,
        outputopaquesubrect: *mut RECT,
    ) -> Result<()> {
        if inputrectcount != 1 {
            return Err(E_INVALIDARG.into());
        }
        unsafe {
            *outputrect = *inputrects;
            *outputopaquesubrect = *inputopaquesubrects;
        }
        Ok(())
    }

    fn MapInvalidRect(&self, _inputindex: u32, invalidinputrect: &RECT) -> Result<RECT> {
        Ok(*invalidinputrect)
    }
}

impl ID2D1DrawTransform_Impl for DitherEffect_Impl {
    fn SetDrawInfo(&self, drawinfo: Ref<'_, ID2D1DrawInfo>) -> Result<()> {
        let info = drawinfo.ok()?;
        match self.pattern {
            DitherPattern::Ordered => unsafe {
                info.SetPixelShader(
                    &shader_identifier(SHADER_ORDERED, self.quantization_steps),
                    D2D1_PIXEL_OPTIONS_NONE,
                )
            },
            DitherPattern::Fruit => {
                unsafe {
                    info.SetPixelShader(
                        &shader_identifier(SHADER_FRUIT, self.quantization_steps),
                        D2D1_PIXEL_OPTIONS_NONE,
                    )
                }?;
                let texture = self.blue_noise_texture.borrow();
                let texture = texture.as_ref().ok_or_else(windows::core::Error::empty)?;
                // Register t1: input 0 occupies t0.
                unsafe { info.SetResourceTexture(1, texture) }
            }
        }
    }
}

fn create_effect(effect: OutRef<'_, IUnknown>, pattern: DitherPattern) -> HRESULT {
    let object: ID2D1EffectImpl = DitherEffect {
        pattern,
        quantization_steps: REGISTERED_QUANTIZATION_STEPS.load(Ordering::Relaxed),
        blue_noise_texture: RefCell::new(None),
    }
    .into();
    match object.cast::<IUnknown>() {
        Ok(unknown) => match effect.write(Some(unknown)) {
            Ok(()) => S_OK,
            Err(error) => error.code(),
        },
        Err(error) => error.code(),
    }
}

unsafe extern "system" fn create_ordered_effect(effect: OutRef<'_, IUnknown>) -> HRESULT {
    create_effect(effect, DitherPattern::Ordered)
}

unsafe extern "system" fn create_fruit_effect(effect: OutRef<'_, IUnknown>) -> HRESULT {
    create_effect(effect, DitherPattern::Fruit)
}

/// Bakes the step count that subsequently created effect instances read.
pub fn prepare_dither_effects(quantization_steps: u32) -> Result<()> {
    compiled_bytecode(DitherPattern::Ordered, quantization_steps)?;
    compiled_bytecode(DitherPattern::Fruit, quantization_steps)?;
    blue_noise_texels();
    REGISTERED_QUANTIZATION_STEPS.store(quantization_steps, Ordering::Relaxed);
    Ok(())
}

/// On failure rendering stays undithered.
pub fn register_dither_effects(factory: &ID2D1Factory1) -> Result<()> {
    let property_xml = w!("<?xml version='1.0'?>\
<Effect>\
<Property name='DisplayName' type='string' value='Dither'/>\
<Property name='Author' type='string' value='riv'/>\
<Property name='Category' type='string' value='Filter'/>\
<Property name='Description' type='string' value='Output quantization dither'/>\
<Inputs><Input name='Source'/></Inputs>\
</Effect>");
    unsafe {
        factory.RegisterEffectFromString(
            &CLSID_RIV_DITHER_ORDERED,
            property_xml,
            None,
            Some(create_ordered_effect),
        )?;
        factory.RegisterEffectFromString(
            &CLSID_RIV_DITHER_FRUIT,
            property_xml,
            None,
            Some(create_fruit_effect),
        )?;
    }
    Ok(())
}
