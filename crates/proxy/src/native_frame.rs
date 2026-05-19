use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder};
use reqwest::header::{CONTENT_RANGE, RANGE, USER_AGENT};
use std::ffi::CStr;
use std::mem::MaybeUninit;
use std::ptr;
use tracing::{debug, trace};

#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    unused_imports,
    dead_code
)]
mod vpx {
    include!(concat!(env!("OUT_DIR"), "/vpx_bindings.rs"));
}

const EBML_ID: u32 = 0x1A45DFA3;
const SEGMENT_ID: u32 = 0x18538067;
const INFO_ID: u32 = 0x1549A966;
const SEEK_HEAD_ID: u32 = 0x114D9B74;
const SEEK_ENTRY_ID: u32 = 0x4DBB;
const SEEK_ID_ID: u32 = 0x53AB;
const SEEK_POSITION_ID: u32 = 0x53AC;
const TIMECODE_SCALE_ID: u32 = 0x2AD7B1;
const TRACKS_ID: u32 = 0x1654AE6B;
const TRACK_ENTRY_ID: u32 = 0xAE;
const TRACK_NUMBER_ID: u32 = 0xD7;
const TRACK_TYPE_ID: u32 = 0x83;
const CODEC_ID_ID: u32 = 0x86;
const CLUSTER_ID: u32 = 0x1F43B675;
const CLUSTER_TIMECODE_ID: u32 = 0xE7;
const SIMPLE_BLOCK_ID: u32 = 0xA3;
const BLOCK_GROUP_ID: u32 = 0xA0;
const BLOCK_ID: u32 = 0xA1;
const REFERENCE_BLOCK_ID: u32 = 0xFB;
const CUES_ID: u32 = 0x1C53BB6B;
const CUE_POINT_ID: u32 = 0xBB;
const CUE_TIME_ID: u32 = 0xB3;
const CUE_TRACK_POSITIONS_ID: u32 = 0xB7;
const CUE_TRACK_ID: u32 = 0xF7;
const CUE_CLUSTER_POSITION_ID: u32 = 0xF1;
const TRACK_TYPE_VIDEO: u64 = 1;
const DEFAULT_TIMECODE_SCALE_NS: u64 = 1_000_000;
const MAX_NATIVE_FRAME_BYTES: u64 = 512 * 1024 * 1024;
const INITIAL_INDEX_RANGE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CUES_RANGE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CLUSTER_RANGE_BYTES: u64 = 128 * 1024 * 1024;
const VPX_CODEC_OK: u32 = 0;
const VPX_IMG_FMT_I420: u32 = 0x100 | 2;

#[derive(Clone, Debug)]
struct Element {
    id: u32,
    data_start: usize,
    data_end: usize,
}

#[derive(Clone, Debug, Default)]
struct VideoTrack {
    number: u64,
    codec_id: String,
}

#[derive(Clone, Debug)]
struct Vp9Frame {
    time_us: u64,
    keyframe: bool,
    data: Vec<u8>,
}

#[derive(Clone, Debug)]
struct CuePoint {
    time_us: u64,
    cluster_position: u64,
    absolute_cluster_offset: u64,
}

#[derive(Clone, Debug)]
struct WebmIndex {
    segment_data_start: u64,
    timecode_scale_ns: u64,
    video_track: VideoTrack,
    cues_seek_position: Option<u64>,
    cues: Vec<CuePoint>,
}

impl Default for WebmIndex {
    fn default() -> Self {
        Self {
            segment_data_start: 0,
            timecode_scale_ns: DEFAULT_TIMECODE_SCALE_NS,
            video_track: VideoTrack::default(),
            cues_seek_position: None,
            cues: Vec::new(),
        }
    }
}

struct RangeFetch {
    start: u64,
    bytes: Vec<u8>,
    total_len: Option<u64>,
}

pub async fn extract_vp9_webm_frame_png(
    client: &reqwest::Client,
    user_agent: &str,
    media_url: &str,
    target_us: u64,
) -> Result<Vec<u8>, String> {
    debug!(target_us, %media_url, "native vp9 webm frame extraction started");
    let head = fetch_range(
        client,
        user_agent,
        media_url,
        0,
        Some(INITIAL_INDEX_RANGE_BYTES - 1),
    )
    .await?;
    if head.start != 0 {
        return Err(format!("initial range started at byte {}", head.start));
    }
    debug!(
        bytes = head.bytes.len(),
        total_len = head.total_len,
        "initial webm index range fetched"
    );

    let mut index = parse_webm_index(&head.bytes)?;
    debug!(
        video_track = index.video_track.number,
        codec = %index.video_track.codec_id,
        timecode_scale_ns = index.timecode_scale_ns,
        segment_data_start = index.segment_data_start,
        cues_in_initial_range = index.cues.len(),
        cues_seek_position = index.cues_seek_position,
        "webm index parsed"
    );
    if index.video_track.number == 0 {
        return Err("WebM index has no video track".to_string());
    }
    if !index.video_track.codec_id.eq_ignore_ascii_case("V_VP9") {
        return Err(format!(
            "native extractor only supports V_VP9, got {}",
            index.video_track.codec_id
        ));
    }

    if index.cues.is_empty() {
        if let Some(position) = index.cues_seek_position {
            let cues_start = index.segment_data_start.saturating_add(position);
            let cues_end = cues_start
                .saturating_add(MAX_CUES_RANGE_BYTES)
                .saturating_sub(1);
            debug!(cues_start, cues_end, "fetching webm cues range");
            let cues_range =
                fetch_range(client, user_agent, media_url, cues_start, Some(cues_end)).await?;
            if cues_range.start != cues_start {
                return Err(format!(
                    "cues range started at byte {}, expected {cues_start}",
                    cues_range.start
                ));
            }
            index.cues = parse_cues_from_range(
                &cues_range.bytes,
                index.video_track.number,
                index.timecode_scale_ns,
                index.segment_data_start,
            );
            debug!(
                bytes = cues_range.bytes.len(),
                cues = index.cues.len(),
                "webm cues range parsed"
            );
        }
    }

    if index.cues.is_empty() {
        return Err("WebM cues are not available for range seeking".to_string());
    }
    index
        .cues
        .sort_by_key(|cue| (cue.time_us, cue.absolute_cluster_offset));

    let cue_index = select_cue_index(&index.cues, target_us);
    let cue = &index.cues[cue_index];
    let cluster_start = cue.absolute_cluster_offset;
    let next_cluster_start = index.cues[cue_index + 1..]
        .iter()
        .map(|next| next.absolute_cluster_offset)
        .find(|next| *next > cluster_start);
    let cluster_end_exclusive = next_cluster_start
        .or(head.total_len)
        .unwrap_or_else(|| cluster_start.saturating_add(MAX_CLUSTER_RANGE_BYTES))
        .min(cluster_start.saturating_add(MAX_CLUSTER_RANGE_BYTES));
    let cluster_end = cluster_end_exclusive.saturating_sub(1).max(cluster_start);
    debug!(
        cue_index,
        cue_time_us = cue.time_us,
        cue_cluster_position = cue.cluster_position,
        cluster_start,
        cluster_end,
        next_cluster_start,
        "selected webm cue and cluster range"
    );
    let cluster_range = fetch_range(
        client,
        user_agent,
        media_url,
        cluster_start,
        Some(cluster_end),
    )
    .await?;

    if cluster_range.start != cluster_start {
        return Err(format!(
            "cluster range started at byte {}, expected {cluster_start}",
            cluster_range.start
        ));
    }

    let frames = parse_cluster_frames_from_range(
        &cluster_range.bytes,
        index.video_track.number,
        index.timecode_scale_ns,
    );
    let keyframes = frames.iter().filter(|frame| frame.keyframe).count();
    debug!(
        bytes = cluster_range.bytes.len(),
        frames = frames.len(),
        keyframes,
        first_frame_us = frames.first().map(|frame| frame.time_us),
        last_frame_us = frames.last().map(|frame| frame.time_us),
        "webm cluster parsed"
    );
    if frames.is_empty() {
        return Err(format!(
            "selected cue at {} us / segment cluster position {} had no decodable video frames",
            cue.time_us, cue.cluster_position
        ));
    }
    tokio::task::spawn_blocking(move || decode_target_frame_to_png(&frames, target_us))
        .await
        .map_err(|error| format!("native frame decode task failed: {error}"))?
}

async fn fetch_range(
    client: &reqwest::Client,
    user_agent: &str,
    media_url: &str,
    start: u64,
    end: Option<u64>,
) -> Result<RangeFetch, String> {
    let range = match end {
        Some(end) => format!("bytes={start}-{end}"),
        None => format!("bytes={start}-"),
    };
    trace!(%range, %media_url, "native frame range fetch request");
    let response = client
        .get(media_url)
        .header(USER_AGENT, user_agent)
        .header(RANGE, range.as_str())
        .send()
        .await
        .map_err(|error| format!("native range fetch failed: {error}"))?;
    let status = response.status();
    trace!(%range, %status, "native frame range fetch response headers received");
    if status.as_u16() != 206 {
        return Err(format!(
            "native range fetch expected HTTP 206, got {status}"
        ));
    }
    let content_range = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("native range body read failed: {error}"))?;
    if bytes.len() as u64 > MAX_NATIVE_FRAME_BYTES {
        return Err(format!(
            "native range input is too large ({} bytes > {MAX_NATIVE_FRAME_BYTES})",
            bytes.len()
        ));
    }
    trace!(
        %range,
        bytes = bytes.len(),
        content_range = content_range.as_deref().unwrap_or(""),
        "native frame range body read"
    );

    let parsed_range = content_range
        .as_deref()
        .and_then(parse_content_range_header)
        .ok_or_else(|| "native range fetch returned an invalid Content-Range".to_string())?;

    Ok(RangeFetch {
        start: parsed_range.0,
        bytes: bytes.to_vec(),
        total_len: parsed_range.2,
    })
}

fn parse_content_range_header(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    let total = if total == "*" {
        None
    } else {
        Some(total.parse::<u64>().ok()?)
    };
    Some((start, end, total))
}

fn parse_webm_index(data: &[u8]) -> Result<WebmIndex, String> {
    if !iter_elements(data, 0, data.len()).any(|element| element.id == EBML_ID) {
        return Err("input is not an EBML/WebM file".to_string());
    }

    let mut index = WebmIndex::default();
    for element in iter_elements(data, 0, data.len()) {
        if element.id != SEGMENT_ID {
            continue;
        }
        index.segment_data_start = element.data_start as u64;
        for child in iter_elements(data, element.data_start, element.data_end) {
            match child.id {
                INFO_ID => {
                    if let Some(value) =
                        parse_timecode_scale(data, child.data_start, child.data_end)
                    {
                        index.timecode_scale_ns = value;
                    }
                }
                TRACKS_ID => {
                    if let Some(track) = parse_video_track(data, child.data_start, child.data_end) {
                        index.video_track = track;
                    }
                }
                SEEK_HEAD_ID => {
                    if let Some(position) =
                        parse_seek_head_cues_position(data, child.data_start, child.data_end)
                    {
                        index.cues_seek_position = Some(position);
                    }
                }
                CUES_ID => {
                    index.cues.extend(parse_cues(
                        data,
                        child.data_start,
                        child.data_end,
                        index.video_track.number,
                        index.timecode_scale_ns,
                        index.segment_data_start,
                    ));
                }
                _ => {}
            }
        }
    }

    Ok(index)
}

fn parse_timecode_scale(data: &[u8], start: usize, end: usize) -> Option<u64> {
    iter_elements(data, start, end)
        .find(|element| element.id == TIMECODE_SCALE_ID)
        .map(|element| read_uint(&data[element.data_start..element.data_end]))
}

fn parse_video_track(data: &[u8], start: usize, end: usize) -> Option<VideoTrack> {
    for entry in iter_elements(data, start, end).filter(|element| element.id == TRACK_ENTRY_ID) {
        let mut number = 0;
        let mut track_type = 0;
        let mut codec_id = String::new();
        for child in iter_elements(data, entry.data_start, entry.data_end) {
            match child.id {
                TRACK_NUMBER_ID => number = read_uint(&data[child.data_start..child.data_end]),
                TRACK_TYPE_ID => track_type = read_uint(&data[child.data_start..child.data_end]),
                CODEC_ID_ID => {
                    codec_id =
                        String::from_utf8_lossy(&data[child.data_start..child.data_end]).to_string()
                }
                _ => {}
            }
        }
        if number != 0 && track_type == TRACK_TYPE_VIDEO {
            return Some(VideoTrack { number, codec_id });
        }
    }
    None
}

fn parse_seek_head_cues_position(data: &[u8], start: usize, end: usize) -> Option<u64> {
    for seek in iter_elements(data, start, end).filter(|element| element.id == SEEK_ENTRY_ID) {
        let mut seek_id = 0;
        let mut seek_position = None;
        for child in iter_elements(data, seek.data_start, seek.data_end) {
            match child.id {
                SEEK_ID_ID => {
                    seek_id = read_raw_element_id(&data[child.data_start..child.data_end]);
                }
                SEEK_POSITION_ID => {
                    seek_position = Some(read_uint(&data[child.data_start..child.data_end]));
                }
                _ => {}
            }
        }
        if seek_id == CUES_ID {
            return seek_position;
        }
    }
    None
}

fn parse_cues_from_range(
    data: &[u8],
    video_track: u64,
    timecode_scale_ns: u64,
    segment_data_start: u64,
) -> Vec<CuePoint> {
    let mut cues = Vec::new();
    for element in iter_elements(data, 0, data.len()) {
        match element.id {
            CUES_ID => cues.extend(parse_cues(
                data,
                element.data_start,
                element.data_end,
                video_track,
                timecode_scale_ns,
                segment_data_start,
            )),
            SEGMENT_ID => {
                for child in iter_elements(data, element.data_start, element.data_end) {
                    if child.id == CUES_ID {
                        cues.extend(parse_cues(
                            data,
                            child.data_start,
                            child.data_end,
                            video_track,
                            timecode_scale_ns,
                            segment_data_start,
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    cues
}

fn parse_cues(
    data: &[u8],
    start: usize,
    end: usize,
    video_track: u64,
    timecode_scale_ns: u64,
    segment_data_start: u64,
) -> Vec<CuePoint> {
    if video_track == 0 {
        return Vec::new();
    }

    let mut cues = Vec::new();
    for point in iter_elements(data, start, end).filter(|element| element.id == CUE_POINT_ID) {
        let mut cue_time = None;
        let mut cue_cluster_position = None;
        for child in iter_elements(data, point.data_start, point.data_end) {
            match child.id {
                CUE_TIME_ID => {
                    cue_time = Some(read_uint(&data[child.data_start..child.data_end]));
                }
                CUE_TRACK_POSITIONS_ID => {
                    let mut cue_track = 0;
                    let mut cluster_position = None;
                    for track_child in iter_elements(data, child.data_start, child.data_end) {
                        match track_child.id {
                            CUE_TRACK_ID => {
                                cue_track =
                                    read_uint(&data[track_child.data_start..track_child.data_end]);
                            }
                            CUE_CLUSTER_POSITION_ID => {
                                cluster_position = Some(read_uint(
                                    &data[track_child.data_start..track_child.data_end],
                                ));
                            }
                            _ => {}
                        }
                    }
                    if cue_track == video_track {
                        cue_cluster_position = cluster_position;
                    }
                }
                _ => {}
            }
        }

        if let (Some(cue_time), Some(cluster_position)) = (cue_time, cue_cluster_position) {
            let time_us = ((cue_time as u128) * (timecode_scale_ns as u128) / 1_000) as u64;
            cues.push(CuePoint {
                time_us,
                cluster_position,
                absolute_cluster_offset: segment_data_start.saturating_add(cluster_position),
            });
        }
    }
    cues
}

fn select_cue_index(cues: &[CuePoint], target_us: u64) -> usize {
    cues.iter()
        .enumerate()
        .take_while(|(_, cue)| cue.time_us <= target_us)
        .map(|(index, _)| index)
        .last()
        .unwrap_or(0)
}

fn parse_cluster_frames_from_range(
    data: &[u8],
    video_track: u64,
    timecode_scale_ns: u64,
) -> Vec<Vp9Frame> {
    let mut frames = Vec::new();
    for element in iter_elements(data, 0, data.len()) {
        if element.id == CLUSTER_ID {
            parse_cluster(
                data,
                element.data_start,
                element.data_end,
                video_track,
                timecode_scale_ns,
                &mut frames,
            );
        }
    }
    frames
}

fn parse_cluster(
    data: &[u8],
    start: usize,
    end: usize,
    video_track: u64,
    timecode_scale_ns: u64,
    frames: &mut Vec<Vp9Frame>,
) {
    let mut cluster_timecode = 0i64;
    for child in iter_elements(data, start, end) {
        match child.id {
            CLUSTER_TIMECODE_ID => {
                cluster_timecode = read_uint(&data[child.data_start..child.data_end]) as i64;
            }
            SIMPLE_BLOCK_ID => {
                if let Some(frame) = parse_block(
                    &data[child.data_start..child.data_end],
                    video_track,
                    cluster_timecode,
                    timecode_scale_ns,
                    None,
                ) {
                    frames.push(frame);
                }
            }
            BLOCK_GROUP_ID => {
                parse_block_group(
                    data,
                    child.data_start,
                    child.data_end,
                    video_track,
                    cluster_timecode,
                    timecode_scale_ns,
                    frames,
                );
            }
            _ => {}
        }
    }
}

fn parse_block_group(
    data: &[u8],
    start: usize,
    end: usize,
    video_track: u64,
    cluster_timecode: i64,
    timecode_scale_ns: u64,
    frames: &mut Vec<Vp9Frame>,
) {
    let mut block: Option<&[u8]> = None;
    let mut has_reference = false;
    for child in iter_elements(data, start, end) {
        match child.id {
            BLOCK_ID => block = Some(&data[child.data_start..child.data_end]),
            REFERENCE_BLOCK_ID => has_reference = true,
            _ => {}
        }
    }
    if let Some(block) = block {
        if let Some(frame) = parse_block(
            block,
            video_track,
            cluster_timecode,
            timecode_scale_ns,
            Some(!has_reference),
        ) {
            frames.push(frame);
        }
    }
}

fn parse_block(
    data: &[u8],
    video_track: u64,
    cluster_timecode: i64,
    timecode_scale_ns: u64,
    keyframe_override: Option<bool>,
) -> Option<Vp9Frame> {
    let (track_number, track_len) = read_vint(data)?;
    if track_number != video_track || data.len() < track_len + 3 {
        return None;
    }
    let relative_timecode = i16::from_be_bytes([data[track_len], data[track_len + 1]]) as i64;
    let flags = data[track_len + 2];
    let lacing = flags & 0x06;
    if lacing != 0 {
        return None;
    }
    let timecode = cluster_timecode + relative_timecode;
    if timecode < 0 {
        return None;
    }
    let time_us = ((timecode as u128) * (timecode_scale_ns as u128) / 1_000) as u64;
    Some(Vp9Frame {
        time_us,
        keyframe: keyframe_override.unwrap_or((flags & 0x80) != 0),
        data: data[track_len + 3..].to_vec(),
    })
}

fn decode_target_frame_to_png(frames: &[Vp9Frame], target_us: u64) -> Result<Vec<u8>, String> {
    let start_index = frames
        .iter()
        .enumerate()
        .take_while(|(_, frame)| frame.time_us <= target_us)
        .filter(|(_, frame)| frame.keyframe)
        .map(|(index, _)| index)
        .last()
        .or_else(|| frames.iter().position(|frame| frame.keyframe))
        .unwrap_or(0);
    debug!(
        target_us,
        frames = frames.len(),
        start_index,
        start_frame_us = frames.get(start_index).map(|frame| frame.time_us),
        "starting vp9 decode from selected keyframe"
    );

    let mut decoder = Vp9Decoder::new()?;
    let mut best: Option<(u64, Vec<u8>, u32, u32)> = None;
    for (relative_index, frame) in frames[start_index..].iter().enumerate() {
        let absolute_index = start_index + relative_index;
        let next_time_us = frames.get(absolute_index + 1).map(|next| next.time_us);
        let should_capture_rgba = frame.time_us >= target_us
            || next_time_us
                .map(|next| next >= target_us)
                .unwrap_or(true);
        trace!(
            frame_time_us = frame.time_us,
            keyframe = frame.keyframe,
            bytes = frame.data.len(),
            capture_rgba = should_capture_rgba,
            "decoding vp9 packet"
        );
        for decoded in decoder.decode(&frame.data, should_capture_rgba)? {
            let distance = frame.time_us.abs_diff(target_us);
            trace!(
                frame_time_us = frame.time_us,
                distance_us = distance,
                width = decoded.width,
                height = decoded.height,
                "vp9 packet produced decoded frame"
            );
            let replace = best
                .as_ref()
                .map(|(best_distance, _, _, _)| distance < *best_distance)
                .unwrap_or(true);
            if replace {
                best = Some((distance, decoded.rgba, decoded.width, decoded.height));
            }
        }
        if frame.time_us >= target_us && best.is_some() {
            break;
        }
    }

    let Some((_distance, rgba, width, height)) = best else {
        return Err("libvpx did not output a frame".to_string());
    };
    let mut png = Vec::new();
    PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Adaptive)
        .write_image(&rgba, width, height, ExtendedColorType::Rgba8)
        .map_err(|error| format!("PNG encode failed: {error}"))?;
    debug!(width, height, bytes = png.len(), "vp9 frame encoded as png");
    Ok(png)
}

struct DecodedFrame {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

struct Vp9Decoder {
    ctx: vpx::vpx_codec_ctx_t,
}

impl Vp9Decoder {
    fn new() -> Result<Self, String> {
        unsafe {
            let mut ctx = MaybeUninit::<vpx::vpx_codec_ctx_t>::zeroed().assume_init();
            let mut cfg = vpx::vpx_codec_dec_cfg {
                threads: 4,
                w: 0,
                h: 0,
            };
            let err = vpx::vpx_codec_dec_init_ver(
                &mut ctx,
                vpx::vpx_codec_vp9_dx(),
                &mut cfg,
                0,
                vpx::VPX_DECODER_ABI_VERSION as i32,
            );
            if err as u32 != VPX_CODEC_OK {
                return Err(codec_error(&ctx, err));
            }
            Ok(Self { ctx })
        }
    }

    fn decode(&mut self, packet: &[u8], capture_rgba: bool) -> Result<Vec<DecodedFrame>, String> {
        unsafe {
            let err = vpx::vpx_codec_decode(
                &mut self.ctx,
                packet.as_ptr(),
                packet.len() as u32,
                ptr::null_mut(),
                0,
            );
            if err as u32 != VPX_CODEC_OK {
                return Err(codec_error(&self.ctx, err));
            }
            let mut out = Vec::new();
            let mut iter: vpx::vpx_codec_iter_t = ptr::null_mut();
            loop {
                let image = vpx::vpx_codec_get_frame(&mut self.ctx, &mut iter);
                if image.is_null() {
                    break;
                }
                if capture_rgba {
                    out.push(image_to_rgba(&*image)?);
                }
            }
            Ok(out)
        }
    }
}

impl Drop for Vp9Decoder {
    fn drop(&mut self) {
        unsafe {
            let _ = vpx::vpx_codec_destroy(&mut self.ctx);
        }
    }
}

fn codec_error(ctx: &vpx::vpx_codec_ctx_t, err: vpx::vpx_codec_err_t) -> String {
    unsafe {
        let message = vpx::vpx_codec_error(ctx);
        if message.is_null() {
            return format!("libvpx error {}", err as u32);
        }
        CStr::from_ptr(message).to_string_lossy().into_owned()
    }
}

fn image_to_rgba(image: &vpx::vpx_image) -> Result<DecodedFrame, String> {
    if image.fmt as u32 != VPX_IMG_FMT_I420 {
        return Err(format!(
            "unsupported libvpx image format {}",
            image.fmt as u32
        ));
    }
    let width = image.d_w;
    let height = image.d_h;
    let y_stride = image.stride[0] as isize;
    let u_stride = image.stride[1] as isize;
    let v_stride = image.stride[2] as isize;
    let y_plane = image.planes[0];
    let u_plane = image.planes[1];
    let v_plane = image.planes[2];
    if y_plane.is_null() || u_plane.is_null() || v_plane.is_null() {
        return Err("libvpx returned a frame with missing planes".to_string());
    }

    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for y in 0..height as usize {
        for x in 0..width as usize {
            unsafe {
                let yy = *y_plane.offset(y as isize * y_stride + x as isize) as i32;
                let uu = *u_plane.offset((y / 2) as isize * u_stride + (x / 2) as isize) as i32;
                let vv = *v_plane.offset((y / 2) as isize * v_stride + (x / 2) as isize) as i32;
                let (r, g, b) = yuv_to_rgb(yy, uu, vv);
                let index = (y * width as usize + x) * 4;
                rgba[index] = r;
                rgba[index + 1] = g;
                rgba[index + 2] = b;
                rgba[index + 3] = 255;
            }
        }
    }
    Ok(DecodedFrame {
        rgba,
        width,
        height,
    })
}

fn yuv_to_rgb(y: i32, u: i32, v: i32) -> (u8, u8, u8) {
    let c = (y - 16).max(0);
    let d = u - 128;
    let e = v - 128;
    let r = (298 * c + 409 * e + 128) >> 8;
    let g = (298 * c - 100 * d - 208 * e + 128) >> 8;
    let b = (298 * c + 516 * d + 128) >> 8;
    (clamp_u8(r), clamp_u8(g), clamp_u8(b))
}

fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

fn iter_elements(data: &[u8], start: usize, end: usize) -> ElementIter<'_> {
    ElementIter {
        data,
        pos: start,
        end: end.min(data.len()),
    }
}

struct ElementIter<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
}

impl Iterator for ElementIter<'_> {
    type Item = Element;

    fn next(&mut self) -> Option<Self::Item> {
        while self.pos < self.end {
            let (id, id_len) = read_element_id(&self.data[self.pos..self.end])?;
            let size_offset = self.pos + id_len;
            let (size, size_len) = read_vint(&self.data[size_offset..self.end])?;
            let data_start = size_offset + size_len;
            let data_end = if is_unknown_size(size, size_len) {
                self.end
            } else {
                data_start.checked_add(size as usize)?.min(self.end)
            };
            self.pos = data_end;
            return Some(Element {
                id,
                data_start,
                data_end,
            });
        }
        None
    }
}

fn read_vint(data: &[u8]) -> Option<(u64, usize)> {
    let first = *data.first()?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || data.len() < len {
        return None;
    }
    let mask = if len < 8 { 0xFFu8 >> len } else { 0 };
    let mut value = (first & mask) as u64;
    for byte in data.iter().take(len).skip(1) {
        value = (value << 8) | *byte as u64;
    }
    Some((value, len))
}

fn is_unknown_size(size: u64, len: usize) -> bool {
    len <= 8 && size == ((1u64 << (7 * len)) - 1)
}

fn read_element_id(data: &[u8]) -> Option<(u32, usize)> {
    let first = *data.first()?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 4 || data.len() < len {
        return None;
    }
    let mut id = 0u32;
    for byte in data.iter().take(len) {
        id = (id << 8) | *byte as u32;
    }
    Some((id, len))
}

fn read_uint(data: &[u8]) -> u64 {
    data.iter()
        .take(8)
        .fold(0u64, |value, byte| (value << 8) | *byte as u64)
}

fn read_raw_element_id(data: &[u8]) -> u32 {
    data.iter()
        .take(4)
        .fold(0u32, |value, byte| (value << 8) | *byte as u32)
}
