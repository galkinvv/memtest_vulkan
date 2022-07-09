use erupt::{
    vk, DeviceLoader, EntryLoader, InstanceLoader,
    extensions::ext_debug_utils,
    cstr,
};
use std::{
    ffi::{c_void, CStr, CString},
    os::raw::c_char,
    fmt,
    mem, 
};

const LAYER_KHRONOS_VALIDATION: *const c_char = cstr!("VK_LAYER_KHRONOS_validation");
const READ_SHADER: &[u32] = memtest_vulkan_build::compiled_vk_compute_spirv!(r#"

struct IOBuffer
{
    single_bit_err_idx: array<u32, 32>,
    err_bits_count: array<u32, 32>,
    max_bad_idx: u32,
    min_bad_idx: u32,
    write_do: u32,
    read_do: u32,
    iteration: u32,
    done_iter_or_status: u32,
    value_calc_param: u32,
}

@group(0) @binding(0) var<storage, read_write> io: IOBuffer;
@group(0) @binding(1) var<storage, read_write> test: array<u32>;

fn test_value_by_index(i:u32)->u32
{
    let result = i + io.value_calc_param;
    if io.read_do != 0 && i == 0xADBAD/4
    {
        return result ^ 4;//error simulation for test
    }
    return result;
}

@compute @workgroup_size(128, 1, 1)
fn main(@builtin(global_invocation_id) global_invocation_id: vec3<u32>) {
    let ITER_CONFIRMATION_VALUE : u32 = 0xFFFFFFu;
    let ERROR_STATUS : u32 = 0xFFFFFFFFu;

    let test_idx = global_invocation_id[0];
    if io.read_do != 0 {
        let expected_value = test_value_by_index(test_idx);
        let actual_value = test[global_invocation_id[0]];
        if actual_value != expected_value {
            //slow path, executed only on errors found
            let error_mask = actual_value ^ expected_value;
            let one_bits = countOneBits(error_mask);
            if one_bits == 1
            {
                let bit_idx = firstLeadingBit(error_mask);
                atomicAdd(&io.single_bit_err_idx[bit_idx], 1u);
            }
            atomicAdd(&io.err_bits_count[one_bits % 32u], 1u);
            atomicMax(&io.max_bad_idx, test_idx);
            atomicMin(&io.min_bad_idx, test_idx);
            atomicMax(&io.done_iter_or_status, ERROR_STATUS);
        }
        //assign done_iter_or_status only on specific index (performance reasons)
        if expected_value == ITER_CONFIRMATION_VALUE {
            atomicMax(&io.done_iter_or_status, io.iteration);
        }
    } else if io.write_do != 0 {
        test[global_invocation_id[0]] = test_value_by_index(global_invocation_id[0]);
    }
}
"#);

const WG_SIZE:u64 = 128;
const ELEMENT_SIZE: u64 = std::mem::size_of::<u32>() as u64;
const ELEMENT_BIT_SIZE: usize = (ELEMENT_SIZE * 8) as usize;
const TEST_WINDOW_SIZE_GRANULARITY: u64 = WG_SIZE * ELEMENT_SIZE;
const TEST_WINDOW_MAX_SIZE: u64= 2*1024*1024*1024 - WG_SIZE * ELEMENT_SIZE;


#[derive(Default)]
struct U64HexDebug(u64);

impl fmt::Debug for U64HexDebug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:X}", self.0)
    }
}

#[derive(Default)]
struct MostlyZeroArr([u32; ELEMENT_BIT_SIZE]);

impl fmt::Debug for MostlyZeroArr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;
        let mut zero_count = 0;
        for i in 0..ELEMENT_BIT_SIZE
        {
            let vali = self.0[i];
            if vali != 0
            {
                write!(f, "[{}]={}, ", i, vali)?;
            }
            else
            {
                zero_count += 1;
            }
        }
        write!(f, "{} zeroes]", zero_count)
    }
}


#[derive(Debug, Default)]
#[repr(C)]
struct IOBuffer
{
    single_bit_err_idx: MostlyZeroArr,
    err_bits_count: MostlyZeroArr,
    max_bad_idx: u32,
    min_bad_idx: u32,
    write_do: u32,
    read_do: u32,
    iteration: u32,
    done_iter_or_status: u32,
    value_calc_param: u32
}

impl IOBuffer
{
    fn prepare_next_iter_write(&mut self)
    {
        *self = IOBuffer { 
            max_bad_idx : u32::MIN,
            min_bad_idx : u32::MAX,
            iteration: self.iteration + 1,
            write_do : 1,
            value_calc_param: (self.iteration + 1).wrapping_mul(0x1000100),
            ..Self::default()
        };
        self.set_calc_param_for_starting_window();
    }
    fn continue_iter_read(&mut self, read_do:u32)
    {
        self.write_do = 0;
        self.read_do = read_do;
        self.set_calc_param_for_starting_window();
    }
    fn set_calc_param_for_starting_window(&mut self)
    {
        self.value_calc_param = self.iteration.wrapping_mul(0x1000100);
    }
    fn reset_errors(&mut self)
    {
        *self = IOBuffer { 
            iteration: self.iteration,
            write_do : self.write_do,
            read_do : self.read_do,
            value_calc_param: self.value_calc_param,
            max_bad_idx : u32::MIN,
            min_bad_idx : u32::MAX,
            ..Self::default()
        };
    }
    fn get_error_addresses(&self, buf_offset:u64) ->  Option<std::ops::RangeInclusive<U64HexDebug>>
    {
        if self.done_iter_or_status == self.iteration
        {
            None
        }
        else
        {
            Some(std::ops::RangeInclusive::<U64HexDebug>::new(
                U64HexDebug(buf_offset + self.min_bad_idx as u64 * ELEMENT_SIZE),
                U64HexDebug(buf_offset + self.max_bad_idx as u64 * ELEMENT_SIZE + ELEMENT_SIZE - 1)
            ))
        }
    }
}

unsafe extern "system" fn debug_callback(
    _message_severity: vk::DebugUtilsMessageSeverityFlagBitsEXT,
    _message_types: vk::DebugUtilsMessageTypeFlagsEXT,
    p_callback_data: *const vk::DebugUtilsMessengerCallbackDataEXT,
    _p_user_data: *mut c_void,
) -> vk::Bool32 {
    eprintln!(
        "{}",
        CStr::from_ptr((*p_callback_data).p_message).to_string_lossy()
    );

    vk::FALSE
}


fn main() -> Result<(), Box<dyn std::error::Error>> {
    let entry = EntryLoader::new()?;
    println!(
        "Running https://github.com/galkinvv/memtest_vulkan on Vulkan Instance {}.{}.{}",
        vk::api_version_major(entry.instance_version()),
        vk::api_version_minor(entry.instance_version()),
        vk::api_version_patch(entry.instance_version())
    );

    let mut instance_extensions = Vec::new();
    let mut instance_layers = Vec::new();
    let mut device_layers = Vec::new();

    instance_extensions.push(ext_debug_utils::EXT_DEBUG_UTILS_EXTENSION_NAME);
    instance_layers.push(LAYER_KHRONOS_VALIDATION);
    device_layers.push(LAYER_KHRONOS_VALIDATION);

    let app_info = vk::ApplicationInfoBuilder::new().
        api_version(vk::API_VERSION_1_1);
    let instance_create_info = vk::InstanceCreateInfoBuilder::new()
        .enabled_extension_names(&instance_extensions)
        .enabled_layer_names(&instance_layers)
        .application_info(&app_info);

    let instance = unsafe { InstanceLoader::new(&entry, &instance_create_info)}?;

    let messenger = {
        let create_info = ext_debug_utils::DebugUtilsMessengerCreateInfoEXTBuilder::new()
            .message_severity(
                ext_debug_utils::DebugUtilsMessageSeverityFlagsEXT::WARNING_EXT
                    | ext_debug_utils::DebugUtilsMessageSeverityFlagsEXT::ERROR_EXT,
                //ext_debug_utils::DebugUtilsMessageSeverityFlagsEXT::VERBOSE_EXT
            )
            .message_type(
                ext_debug_utils::DebugUtilsMessageTypeFlagsEXT::GENERAL_EXT
                    | ext_debug_utils::DebugUtilsMessageTypeFlagsEXT::VALIDATION_EXT
                    | ext_debug_utils::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE_EXT,
            )
            .pfn_user_callback(Some(debug_callback));

        unsafe { instance.create_debug_utils_messenger_ext(&create_info, None) }.unwrap()
    };


    let (physical_device, queue_family, properties) =
        unsafe { instance.enumerate_physical_devices(None) }
            .unwrap()
            .into_iter()
            .filter_map(|physical_device| unsafe {
                let queue_family = match instance
                    .get_physical_device_queue_family_properties(physical_device, None)
                    .into_iter()
                    .position(|properties| {
                        properties.queue_flags.contains(vk::QueueFlags::COMPUTE)
                    }) {
                    Some(queue_family) => queue_family as u32,
                    None => return None,
                };

                let properties = instance.get_physical_device_properties(physical_device);
                Some((physical_device, queue_family, properties))
            })
            .max_by_key(|(_, _, properties)| match properties.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 2,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
                _ => 0,
            })
            .expect("No suitable physical device found");

    println!("Using physical device: {:?}", unsafe {
        CStr::from_ptr(properties.device_name.as_ptr())
    });

    let memory_props = unsafe { instance.get_physical_device_memory_properties(physical_device) };

    let queue_create_info = vec![vk::DeviceQueueCreateInfoBuilder::new()
        .queue_family_index(queue_family)
        .queue_priorities(&[1.0])];
    let features = vk::PhysicalDeviceFeaturesBuilder::new();

    let device_create_info = vk::DeviceCreateInfoBuilder::new()
        .queue_create_infos(&queue_create_info)
        .enabled_features(&features)
        .enabled_layer_names(&device_layers);

    let device = unsafe { DeviceLoader::new(&instance, physical_device, &device_create_info)}?;
    let queue = unsafe { device.get_device_queue(queue_family, 0) };

    let io_data_size = mem::size_of::<IOBuffer>() as vk::DeviceSize;

    let io_buffer_create_info = vk::BufferCreateInfoBuilder::new()
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
        .size(io_data_size);
    let io_buffer = unsafe {device.create_buffer(&io_buffer_create_info, None)}.unwrap();
    let io_mem_reqs = unsafe {device.get_buffer_memory_requirements(io_buffer)};
    let io_mem_index = (0..memory_props.memory_type_count)
        .find(|i| {
            //test buffer comptibility flags expressed as bitmask
            let suitable = (io_mem_reqs.memory_type_bits & (1 << i)) != 0;
            let memory_type = memory_props.memory_types[*i as usize];
            suitable && memory_type.property_flags.contains(
                vk::MemoryPropertyFlags::DEVICE_LOCAL | vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT)
        }).ok_or("DEVICE_LOCAL | HOST_COHERENT memory type not available")?;
    println!("IO memory type: index {}: {:?} heap {:?}", io_mem_index, memory_props.memory_types[io_mem_index as usize], memory_props.memory_heaps[memory_props.memory_types[io_mem_index as usize].heap_index as usize]);

    let io_memory_allocate_info = vk::MemoryAllocateInfoBuilder::new()
        .allocation_size(io_mem_reqs.size)
        .memory_type_index(io_mem_index);
    let io_memory = unsafe{device.allocate_memory(&io_memory_allocate_info, None)}.unwrap();

    let mapped: *mut IOBuffer = unsafe{mem::transmute(device.map_memory(io_memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::default()).unwrap())};
    unsafe{device.bind_buffer_memory(io_buffer, io_memory, 0)}.unwrap();

    let test_data_size = 5u64*1024*1024*1024;
    let test_window_count = test_data_size / TEST_WINDOW_MAX_SIZE + u64::from(test_data_size % TEST_WINDOW_MAX_SIZE != 0);
    let test_window_size = test_data_size / test_window_count;
    let test_window_size = test_window_size - test_window_size % TEST_WINDOW_SIZE_GRANULARITY;
    let test_data_size = test_window_size * test_window_count;

    let test_buffer_create_info = vk::BufferCreateInfoBuilder::new()
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
        .size(test_data_size);
    let test_buffer = unsafe {device.create_buffer(&test_buffer_create_info, None)}.unwrap();
    let test_mem_reqs = unsafe {device.get_buffer_memory_requirements(test_buffer)};
    let test_mem_index = (0..memory_props.memory_type_count)
        .find(|i| {
            //test buffer comptibility flags expressed as bitmask
            let suitable = (test_mem_reqs.memory_type_bits & (1 << i)) != 0;
            let memory_type = memory_props.memory_types[*i as usize];
            let memory_heap = memory_props.memory_heaps[memory_type.heap_index as usize];
            suitable && memory_heap.size >= test_data_size && memory_type.property_flags.contains(
                vk::MemoryPropertyFlags::DEVICE_LOCAL)
        }).ok_or("DEVICE_LOCAL test memory type not available")?;
    println!("Test memory type: index {}: {:?} heap {:?}", test_mem_index, memory_props.memory_types[test_mem_index as usize], memory_props.memory_heaps[memory_props.memory_types[test_mem_index as usize].heap_index as usize]);

    let test_memory_allocate_info = vk::MemoryAllocateInfoBuilder::new()
        .allocation_size(test_mem_reqs.size)
        .memory_type_index(test_mem_index);
    let test_memory = unsafe{device.allocate_memory(&test_memory_allocate_info, None)}.unwrap();
    unsafe{device.bind_buffer_memory(test_buffer, test_memory, 0)}.unwrap();

    let desc_pool_sizes = &[vk::DescriptorPoolSizeBuilder::new()
        .descriptor_count(2)
        ._type(vk::DescriptorType::STORAGE_BUFFER)];
    let desc_pool_info = vk::DescriptorPoolCreateInfoBuilder::new()
        .pool_sizes(desc_pool_sizes)
        .max_sets(1);
    let desc_pool = unsafe { device.create_descriptor_pool(&desc_pool_info, None) }.unwrap();

    let desc_layout_bindings = &[vk::DescriptorSetLayoutBindingBuilder::new()
        .binding(0)
        .descriptor_count(1)
        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
        .stage_flags(vk::ShaderStageFlags::COMPUTE),

        vk::DescriptorSetLayoutBindingBuilder::new()
        .binding(1)
        .descriptor_count(1)
        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
        .stage_flags(vk::ShaderStageFlags::COMPUTE)
        ];
    let desc_layout_info =
        vk::DescriptorSetLayoutCreateInfoBuilder::new().bindings(desc_layout_bindings);
    let desc_layout =
        unsafe { device.create_descriptor_set_layout(&desc_layout_info, None) }.unwrap();

    let desc_layouts = &[desc_layout];
    let desc_info = vk::DescriptorSetAllocateInfoBuilder::new()
        .descriptor_pool(desc_pool)
        .set_layouts(desc_layouts);
    let desc_set = unsafe { device.allocate_descriptor_sets(&desc_info) }.unwrap()[0];

    unsafe {
        device.update_descriptor_sets(
            &[
            vk::WriteDescriptorSetBuilder::new()
                .dst_set(desc_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&[vk::DescriptorBufferInfoBuilder::new()
                    .buffer(io_buffer)
                    .offset(0)
                    .range(vk::WHOLE_SIZE)]),
            ],
            &[],
        );
    }

    let pipeline_layout_desc_layouts = &[desc_layout];
    let pipeline_layout_info =
        vk::PipelineLayoutCreateInfoBuilder::new().set_layouts(pipeline_layout_desc_layouts);
    let pipeline_layout =
        unsafe { device.create_pipeline_layout(&pipeline_layout_info, None) }.unwrap();

    let read_spv_code = Vec::from(READ_SHADER);
    let read_create_info = vk::ShaderModuleCreateInfoBuilder::new().code(&read_spv_code);
    let read_shader_mod = unsafe { device.create_shader_module(&read_create_info, None) }.unwrap();

    let entry_point = CString::new("main")?;
    let read_shader_stage = vk::PipelineShaderStageCreateInfoBuilder::new()
        .stage(vk::ShaderStageFlagBits::COMPUTE)
        .module(read_shader_mod)
        .name(&entry_point);

    let pipeline_info = &[vk::ComputePipelineCreateInfoBuilder::new()
        .layout(pipeline_layout)
        .stage(*read_shader_stage)];
    let pipeline =
        unsafe { device.create_compute_pipelines(Default::default(), pipeline_info, None) }.unwrap()[0];

    let cmd_pool_info = vk::CommandPoolCreateInfoBuilder::new()
        .queue_family_index(queue_family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    let cmd_pool = unsafe { device.create_command_pool(&cmd_pool_info, None) }.unwrap();

    let cmd_buf_info = vk::CommandBufferAllocateInfoBuilder::new()
        .command_pool(cmd_pool)
        .command_buffer_count(1)
        .level(vk::CommandBufferLevel::PRIMARY);
    let cmd_buf = unsafe { device.allocate_command_buffers(&cmd_buf_info) }.unwrap()[0];

    let test_element_count = (test_window_size/ELEMENT_SIZE) as u32;

    let fence_info = vk::FenceCreateInfoBuilder::new();
    let fence = unsafe { device.create_fence(&fence_info, None) }.unwrap();

    let cmd_bufs = &[cmd_buf];
    let submit_info = &[vk::SubmitInfoBuilder::new().command_buffers(cmd_bufs)];
    let execute_wait_queue = |buf_offset: u64| unsafe {
        let now = std::time::Instant::now();
        device.update_descriptor_sets(
            &[
            vk::WriteDescriptorSetBuilder::new()
                .dst_set(desc_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&[vk::DescriptorBufferInfoBuilder::new()
                    .buffer(test_buffer)
                    .offset(buf_offset)
                    .range(test_window_size)]),
            ],
            &[],
        );
        device.begin_command_buffer(cmd_buf, &vk::CommandBufferBeginInfo::default()).unwrap();
        device.cmd_bind_pipeline(cmd_buf, vk::PipelineBindPoint::COMPUTE, pipeline);
        device.cmd_bind_descriptor_sets(
            cmd_buf,
            vk::PipelineBindPoint::COMPUTE,
            pipeline_layout,
            0,
            &[desc_set],
            &[],
        );
        device.cmd_dispatch(cmd_buf, test_element_count/WG_SIZE as u32, 1, 1);
        device.end_command_buffer(cmd_buf).unwrap();
        device.queue_submit(queue, submit_info, fence).unwrap();
        device.wait_for_fences(&[fence], true, u64::MAX).unwrap();
        device.reset_fences(&[fence]).unwrap();
        now.elapsed()
    };
    {
        let mut buffer_in = IOBuffer::default();
        buffer_in.prepare_next_iter_write();
        println!("input: {:?}", buffer_in);
        unsafe {
            std::ptr::write(mapped, buffer_in)
        }
    }
    for window_idx in 0..test_window_count
    {
        let test_offset = test_window_size * window_idx;

        unsafe {
            (*mapped).value_calc_param += window_idx as u32 * 0x81;
        }
        let write_exec_duration = execute_wait_queue(test_offset);
        unsafe {
            let buffer_status = std::ptr::read(mapped);
            println!("medium: {:?} {:?}", buffer_status, write_exec_duration);
        }
    }
    unsafe {
        let mut buffer_in_out = std::ptr::read(mapped);
        buffer_in_out.continue_iter_read(1);
        std::ptr::write(mapped, buffer_in_out);
    }
    for window_idx in 0..test_window_count
    {
        unsafe {
            let mut buffer_in_out = std::ptr::read(mapped);
            buffer_in_out.reset_errors();
            buffer_in_out.value_calc_param += window_idx as u32 * 0x81;
            std::ptr::write(mapped, buffer_in_out);
        }
        let test_offset = test_window_size * window_idx;
        let read_exec_duration = execute_wait_queue(test_offset);
        {
            let buffer_out : IOBuffer;
            unsafe {
                buffer_out = std::ptr::read(mapped);
            }
            println!("output: {:?} {:?}", buffer_out, read_exec_duration);
            if let Some(error) = buffer_out.get_error_addresses(test_offset)
            {
                println!("error addresses: {:?}", error);
                
            }
        }
    }
    // Cleanup & Destruction
    unsafe {
        device.device_wait_idle().unwrap();

        device.destroy_buffer(test_buffer, None);
        device.free_memory(test_memory, None);

        device.destroy_buffer(io_buffer, None);
        device.unmap_memory(io_memory);
        device.free_memory(io_memory, None);

        device.destroy_pipeline(pipeline, None);
        device.destroy_pipeline_layout(pipeline_layout, None);
        device.destroy_command_pool(cmd_pool, None);
        device.destroy_fence(fence, None);
        device.destroy_descriptor_set_layout(desc_layout, None);
        device.destroy_descriptor_pool(desc_pool, None);
        device.destroy_shader_module(read_shader_mod, None);
        device.destroy_device(None);

        instance.destroy_debug_utils_messenger_ext(messenger, None);
        instance.destroy_instance(None);
    }

    println!("Exited cleanly");
    Ok(())
}
