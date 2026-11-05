//! End-to-end test of the client against the in-process mock camera.
use openfuji::camera::{Camera, ConnectOptions, Mode};
use openfuji::dataset::PropValue;
use openfuji::ptp::prop;
use std::time::Duration;

#[test]
fn full_session_against_mock() {
    // A loopback address of our own so we don't clash with a running mock.
    let ip = "127.0.0.77".parse().unwrap();
    std::thread::spawn(move || openfuji::mock::MockCamera::new(3).serve(ip).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::net::TcpStream::connect((ip, openfuji::ptp::LIVEVIEW_PORT)).is_err() {
        assert!(std::time::Instant::now() < deadline, "mock did not start");
        std::thread::sleep(Duration::from_millis(100));
    }

    let opts = ConnectOptions {
        ip,
        name: "test".into(),
        ..Default::default()
    };

    // Browse mode: list + download with both methods.
    let mut cam = Camera::connect(&opts).unwrap();
    assert_eq!(cam.ack.camera_name, "X-MOCK");
    assert_eq!(cam.device_info().unwrap().model, "X-MOCK");
    cam.set_mode(Mode::PhotoView).unwrap();
    assert_eq!(cam.prop_u16(prop::FUJI_FUNCTION_MODE).unwrap(), 2);
    let handles = cam.object_handles().unwrap();
    assert_eq!(handles, vec![1, 2, 3]);
    let info = cam.object_info(2).unwrap();
    assert_eq!(info.filename, "DSCF0002.JPG");
    let mut a = Vec::new();
    cam.download(2, &mut a, &mut |_| {}).unwrap();
    assert_eq!(a.len() as u32, info.compressed_size);
    assert_eq!(&a[..2], &[0xff, 0xd8]);
    let mut b = Vec::new();
    cam.download_partial(2, a.len() as u64, 1000, &mut b, &mut |_| {})
        .unwrap();
    assert_eq!(a, b);
    assert!(!cam.thumbnail(1).unwrap().is_empty());
    cam.close().unwrap();

    // Remote mode: properties, live view, capture.
    let mut cam = Camera::connect(&opts).unwrap();
    cam.set_mode(Mode::Remote).unwrap();
    assert_eq!(cam.prop_u16(prop::FUJI_FUNCTION_MODE).unwrap(), 5);
    cam.set_prop_typed(prop::F_NUMBER, &PropValue::UInt(800))
        .unwrap();
    let d = cam.prop_desc(prop::F_NUMBER).unwrap();
    assert_eq!(d.current, PropValue::UInt(800));
    cam.set_prop_typed(prop::EXPOSURE_BIAS, &PropValue::Int(-667))
        .unwrap();
    assert_eq!(
        cam.prop_desc(prop::EXPOSURE_BIAS).unwrap().current,
        PropValue::Int(-667)
    );
    let changes = cam.poll_events().unwrap();
    assert!(
        changes
            .iter()
            .any(|&(c, v)| c == prop::F_NUMBER && v == 800)
    );

    let (tid, mut lv, mut ev) = cam.start_liveview().unwrap();
    for _ in 0..3 {
        let f = lv.next_frame().unwrap();
        assert_eq!(&f.jpeg[..2], &[0xff, 0xd8]);
        assert_eq!(&f.jpeg[f.jpeg.len() - 2..], &[0xff, 0xd9]);
        assert_eq!(f.header.len(), 14);
    }
    cam.s1_lock().unwrap();
    cam.capture().unwrap();
    cam.s1_unlock().unwrap();
    let e = ev.next_event().unwrap();
    assert_eq!(e.code, 0x4002);
    assert_eq!(e.params, vec![4]);
    assert_eq!(cam.prop_u32(prop::FUJI_OBJECT_COUNT).unwrap(), 4);
    cam.stop_liveview(tid).unwrap();
    cam.close().unwrap();
}
