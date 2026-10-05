//! Persistent WGC sessions share one D3D11 device. A consumer requests a frame
//! only after the preceding frame has been consumed; the capture pool is bounded.
use std::{marker::PhantomData, rc::Rc, time::{Duration,Instant}, sync::{Arc, atomic::{AtomicBool, Ordering}}};
use windows::{core::{Interface, Result}, Foundation::TypedEventHandler,
    Graphics::{Capture::*, DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat}, SizeInt32},
    Win32::{Foundation::*, Graphics::{Direct3D::*, Direct3D11::*, Dxgi::{IDXGIDevice, Common::*}},
        System::WinRT::{*, Direct3D11::*, Graphics::Capture::IGraphicsCaptureItemInterop}}};

const MAX_PIXELS: u64 = 16_777_216;
const FRAME_BUFFERS: i32 = 1;

struct Apartment(PhantomData<Rc<()>>);
impl Drop for Apartment { fn drop(&mut self) { unsafe { RoUninitialize() }; } }

pub(super) struct Device {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    runtime: IDirect3DDevice,
    wake: Option<Arc<super::wait::Wake>>,
    _apartment: Apartment,
}
impl Device {
    pub fn new(wake:Option<Arc<super::wait::Wake>>) -> Result<Rc<Self>> { unsafe {
        RoInitialize(RO_INIT_MULTITHREADED)?;
        let apartment = Apartment(PhantomData);
        // Avoid the generated global factory cache surviving its COM apartment.
        let factory = windows::core::factory::<GraphicsCaptureSession, IGraphicsCaptureSessionStatics>()?;
        let mut supported = false;
        (factory.vtable().IsSupported)(factory.as_raw(), &mut supported).ok()?;
        if !supported { return Err(E_NOTIMPL.into()); }
        let (mut device, mut context) = (None, None);
        D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None, D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut context))?;
        let device = device.ok_or(E_POINTER)?;
        let context = context.ok_or(E_POINTER)?;
        let _ = context.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
        let runtime = CreateDirect3D11DeviceFromDXGIDevice(&device.cast::<IDXGIDevice>()?)?.cast()?;
        Ok(Rc::new(Self { device, context, runtime, wake, _apartment:apartment }))
    } }
}

fn bounded(size:SizeInt32) -> Result<(u32,u32)> {
    if size.Width < 1 || size.Height < 1 || size.Width > 8192 || size.Height > 8192
        || size.Width as u64 * size.Height as u64 > MAX_PIXELS { return Err(E_INVALIDARG.into()); }
    Ok((size.Width as u32,size.Height as u32))
}

pub(super) struct Picture { pub size:(u32,u32), pub pixels:Vec<u8> }
struct Frame(Direct3D11CaptureFrame);
impl Drop for Frame { fn drop(&mut self) { let _ = self.0.Close(); } }
struct Mapped<'a>(&'a ID3D11DeviceContext,&'a ID3D11Texture2D);
impl Drop for Mapped<'_> { fn drop(&mut self) { unsafe { self.0.Unmap(self.1,0) }; } }

pub(super) struct Capture {
    // Drop WinRT objects before the device/apartment, including on failure.
    item: GraphicsCaptureItem,
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    closed_token: i64,
    frame_token: i64,
    closed: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
    pending: bool,
    pending_since: Instant,
    staging: Option<ID3D11Texture2D>,
    size: SizeInt32,
    device: Rc<Device>,
}
impl Capture {
    pub fn new(device:Rc<Device>, hwnd:HWND, budget:u64) -> Result<Self> { unsafe {
        let interop:IGraphicsCaptureItemInterop = windows::core::factory::<GraphicsCaptureItem,_>()?;
        let item:GraphicsCaptureItem = interop.CreateForWindow(hwnd)?;
        let size = item.Size()?;
        bounded(size)?;
        if size.Width as u64 * size.Height as u64 > budget {
            return Err(windows::core::Error::new(E_OUTOFMEMORY,"aggregate window capture budget exceeded"));
        }
        Self::from_item(device,item,size)
    } }
    fn from_item(device:Rc<Device>,item:GraphicsCaptureItem,size:SizeInt32) -> Result<Self> { unsafe {
        bounded(size)?;
        let factory = windows::core::factory::<Direct3D11CaptureFramePool,IDirect3D11CaptureFramePoolStatics2>()?;
        let mut raw = std::ptr::null_mut();
        (factory.vtable().CreateFreeThreaded)(factory.as_raw(), device.runtime.as_raw(),
            DirectXPixelFormat::B8G8R8A8UIntNormalized, FRAME_BUFFERS, size, &mut raw).ok()?;
        if raw.is_null() { return Err(E_POINTER.into()); }
        let pool = Direct3D11CaptureFramePool::from_raw(raw);
        let session = match pool.CreateCaptureSession(&item) {
            Ok(session) => session,
            Err(error) => { let _ = pool.Close(); return Err(error); }
        };
        let mut capture = Self { item, session, pool, closed_token:0, frame_token:0,
            closed:Arc::new(AtomicBool::new(false)), dirty:Arc::new(AtomicBool::new(true)),
            pending:false, pending_since:Instant::now(), staging:None, size, device };
        let closed = capture.closed.clone();
        let wake = capture.device.wake.clone();
        capture.closed_token = capture.item.Closed(&TypedEventHandler::new(move |_,_| {
            closed.store(true,Ordering::Release); if let Some(wake)=&wake { wake.signal(); } Ok(())
        }))?;
        let dirty = capture.dirty.clone();
        let wake = capture.device.wake.clone();
        capture.frame_token = capture.pool.FrameArrived(&TypedEventHandler::new(move |_,_| {
            dirty.store(true,Ordering::Release); if let Some(wake)=&wake { wake.signal(); } Ok(())
        }))?;
        capture.session.SetIsCursorCaptureEnabled(false)?;
        capture.session.StartCapture()?;
        Ok(capture)
    } }

    pub fn size(&self) -> Result<(u32,u32)> { bounded(self.size) }
    pub fn closed(&self) -> bool { self.closed.load(Ordering::Acquire) }
    pub fn pending(&self) -> bool { self.pending }
    pub fn ready(&self) -> bool { self.pending || self.closed() || self.dirty.load(Ordering::Acquire) }

    /// No blocking readback: a busy GPU is retried on the next consumer tick.
    /// One WGC buffer, one staging texture and one CPU result per window.
    pub fn next(&mut self, budget:u64) -> Result<Option<Picture>> { unsafe {
        if self.closed() { return Err(RO_E_CLOSED.into()); }
        if self.size.Width as u64 * self.size.Height as u64 > budget {
            return Err(windows::core::Error::new(E_OUTOFMEMORY,"aggregate window capture budget exceeded"));
        }
        if !self.pending {
            if !self.dirty.swap(false,Ordering::AcqRel) { return Ok(None); }
            let frame = match self.pool.TryGetNextFrame() {
                Ok(frame) => Frame(frame),
                Err(e) if e.code() == E_POINTER || e.code() == S_OK => return Ok(None),
                Err(e) => return Err(e),
            };
            let content = frame.0.ContentSize()?;
            bounded(content)?;
            if content.Width as u64 * content.Height as u64 > budget {
                return Err(windows::core::Error::new(E_OUTOFMEMORY,"aggregate window capture budget exceeded"));
            }
            if content != self.size {
                #[cfg(test)] eprintln!("capture resize: {:?} -> {:?}",self.size,content);
                drop(frame);
                self.staging = None;
                // Recreate can discard the only update from a static source.
                // A fresh pool/session requests the complete resized picture;
                // reuse the capture item, never reopen a possibly recycled HWND.
                self.session.Close()?;
                *self=Self::from_item(self.device.clone(),self.item.clone(),content)?;
                return Ok(None);
            }
            let (width,height) = bounded(self.size)?;
            if self.staging.is_none() {
                self.device.device.CreateTexture2D(&D3D11_TEXTURE2D_DESC {
                    Width:width, Height:height, MipLevels:1, ArraySize:1,
                    Format:DXGI_FORMAT_B8G8R8A8_UNORM, SampleDesc:DXGI_SAMPLE_DESC {Count:1,Quality:0},
                    Usage:D3D11_USAGE_STAGING, CPUAccessFlags:D3D11_CPU_ACCESS_READ.0 as u32,
                    ..Default::default()
                },None,Some(&mut self.staging))?;
            }
            let source:ID3D11Texture2D = frame.0.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?.GetInterface()?;
            self.device.context.CopyResource(self.staging.as_ref().ok_or(E_POINTER)?,&source);
            self.device.context.Flush();
            // The submitted GPU copy owns its resources. Return the WGC buffer
            // now: holding it during CPU readback can lose a static repaint.
            drop(frame);
            self.pending = true;
            self.pending_since = Instant::now();
        }
        let texture = self.staging.as_ref().ok_or(E_POINTER)?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        match self.device.context.Map(texture,0,D3D11_MAP_READ,D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,Some(&mut mapped)) {
            Ok(()) => {},
            Err(e) if e.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING => {
                if self.pending_since.elapsed()>Duration::from_secs(5) {
                    return Err(windows::core::Error::new(E_FAIL,"window capture GPU readback timed out"));
                }
                return Ok(None);
            },
            Err(e) => return Err(e),
        }
        let mapping = Mapped(&self.device.context,texture);
        let (width,height) = bounded(self.size)?;
        let stride = width as usize * 4;
        if mapped.pData.is_null() || (mapped.RowPitch as usize) < stride { return Err(E_FAIL.into()); }
        let mut pixels = vec![0u8;stride * height as usize];
        for y in 0..height as usize {
            let row = std::slice::from_raw_parts(mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize),stride);
            pixels[y*stride..(y+1)*stride].copy_from_slice(row);
        }
        drop(mapping);
        self.pending = false;
        Ok(Some(Picture { size:(width,height),pixels }))
    } }
}
impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        if self.frame_token != 0 { let _ = self.pool.RemoveFrameArrived(self.frame_token); }
        if self.closed_token != 0 { let _ = self.item.RemoveClosed(self.closed_token); }
        let _ = self.pool.Close();
    }
}
