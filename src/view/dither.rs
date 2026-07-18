//! Custom D2D output-dither effects: Ordered (Bayer 8x8) and Fruit (blue noise).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, RECT, S_OK};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BUFFER_PRECISION_8BPC_UNORM, D2D1_CHANGE_TYPE, D2D1_CHANNEL_DEPTH_1,
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
const BLUE_NOISE_TEXTURE: GUID = GUID::from_u128(0x417f9d25_c680_4e8a_b1d7_063e94a8c5f2);

const BLUE_NOISE_SIZE: u32 = 64;
const BLUE_NOISE_PNG: &[u8] = include_bytes!("../../res/blue_noise_64.png");

const ORDERED_SHADER_SOURCE: &str = "\
Texture2D input_texture : register(t0);
SamplerState input_sampler : register(s0);

static const float bayer_8x8[64] = {
     0.0, 32.0,  8.0, 40.0,  2.0, 34.0, 10.0, 42.0,
    48.0, 16.0, 56.0, 24.0, 50.0, 18.0, 58.0, 26.0,
    12.0, 44.0,  4.0, 36.0, 14.0, 46.0,  6.0, 38.0,
    60.0, 28.0, 52.0, 20.0, 62.0, 30.0, 54.0, 22.0,
     3.0, 35.0, 11.0, 43.0,  1.0, 33.0,  9.0, 41.0,
    51.0, 19.0, 59.0, 27.0, 49.0, 17.0, 57.0, 25.0,
    15.0, 47.0,  7.0, 39.0, 13.0, 45.0,  5.0, 37.0,
    63.0, 31.0, 55.0, 23.0, 61.0, 29.0, 53.0, 21.0 };

float4 main(float4 clip_position : SV_POSITION,
            float4 scene_position : SCENE_POSITION,
            float4 texture_coordinate : TEXCOORD0) : SV_Target
{
    float4 color = input_texture.Sample(input_sampler, texture_coordinate.xy);
    uint2 cell = uint2(scene_position.xy) & 7u;
    float threshold = (bayer_8x8[cell.y * 8u + cell.x] + 0.5) / 64.0 - 0.5;
    color.rgb += threshold / QUANTIZATION_STEPS;
    return color;
}
";

const FRUIT_SHADER_SOURCE: &str = "\
Texture2D input_texture : register(t0);
SamplerState input_sampler : register(s0);
Texture2D blue_noise_texture : register(t1);
SamplerState blue_noise_sampler : register(s1);

float4 main(float4 clip_position : SV_POSITION,
            float4 scene_position : SCENE_POSITION,
            float4 texture_coordinate : TEXCOORD0) : SV_Target
{
    float4 color = input_texture.Sample(input_sampler, texture_coordinate.xy);
    float noise = blue_noise_texture.Sample(blue_noise_sampler, scene_position.xy / 64.0).r;
    color.rgb += (noise - 0.5) / QUANTIZATION_STEPS;
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
    let source = match pattern {
        DitherPattern::Ordered => ORDERED_SHADER_SOURCE,
        DitherPattern::Fruit => FRUIT_SHADER_SOURCE,
    }
    .replace("QUANTIZATION_STEPS", &format!("{quantization_steps}.0"));
    let bytecode: &'static [u8] =
        Box::leak(compile_shader(&source, s!("ps_5_0"))?.into_boxed_slice());
    cache.insert(key, bytecode);
    Ok(bytecode)
}

/// Single-channel 8-bit texels decoded from the embedded CC0 texture.
fn blue_noise_texels() -> Result<&'static [u8]> {
    static TEXELS: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    TEXELS
        .get_or_init(|| {
            let mut decoder = png::Decoder::new(std::io::Cursor::new(BLUE_NOISE_PNG));
            decoder.set_transformations(png::Transformations::normalize_to_color8());
            let mut reader = decoder.read_info().ok()?;
            let mut buffer = vec![0u8; reader.output_buffer_size()?];
            reader.next_frame(&mut buffer).ok()?;
            let (color_type, _) = reader.output_color_type();
            let samples = color_type.samples();
            let texels: Vec<u8> = buffer.chunks_exact(samples).map(|pixel| pixel[0]).collect();
            (texels.len() == (BLUE_NOISE_SIZE * BLUE_NOISE_SIZE) as usize).then_some(texels)
        })
        .as_deref()
        .ok_or_else(windows::core::Error::empty)
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
                    &SHADER_ORDERED,
                    compiled_bytecode(DitherPattern::Ordered, self.quantization_steps)?,
                )?;
            },
            DitherPattern::Fruit => {
                unsafe {
                    context.LoadPixelShader(
                        &SHADER_FRUIT,
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
                            bufferPrecision: D2D1_BUFFER_PRECISION_8BPC_UNORM,
                            channelDepth: D2D1_CHANNEL_DEPTH_1,
                            filter: D2D1_FILTER_MIN_MAG_MIP_POINT,
                            extendModes: extend_modes.as_ptr(),
                        };
                        let strides = [BLUE_NOISE_SIZE];
                        unsafe {
                            context.CreateResourceTexture(
                                Some(&BLUE_NOISE_TEXTURE),
                                &raw const properties,
                                Some(blue_noise_texels()?),
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
                info.SetPixelShader(&SHADER_ORDERED, D2D1_PIXEL_OPTIONS_NONE)
            },
            DitherPattern::Fruit => {
                unsafe { info.SetPixelShader(&SHADER_FRUIT, D2D1_PIXEL_OPTIONS_NONE) }?;
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

/// On failure rendering stays undithered.
pub fn register_dither_effects(factory: &ID2D1Factory1, quantization_steps: u32) -> Result<()> {
    compiled_bytecode(DitherPattern::Ordered, quantization_steps)?;
    compiled_bytecode(DitherPattern::Fruit, quantization_steps)?;
    blue_noise_texels()?;
    REGISTERED_QUANTIZATION_STEPS.store(quantization_steps, Ordering::Relaxed);
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
