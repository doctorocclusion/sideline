use cgmath::prelude::*;
use cgmath::{Point3, Matrix4, Deg, PerspectiveFov, vec3};
use image;
use gfx;
use gfx::traits::{FactoryExt};
use gfx::texture;
use gfx::format;
use gfx::handle::*;
use gfx_app::{self, ApplicationBase};
use winit::{self, Event};
use std::time::Instant;
use std::path::{Path, PathBuf};

use shaders;
use define::{self, VertexSlice};
use camera::{Camera, BasicCamera, ArcBall};
use wavefront::{open_obj};
use rand::{Rng, ThreadRng, thread_rng};

pub struct App<R: gfx::Resources, C: gfx::CommandBuffer<R>> {
    //=======//
    // Input //
    //=======//
    mouse_pos: Option<(i32, i32)>,
    orbit_diff: (f32, f32, f32),
    left_down: bool,
    cam: ArcBall<PerspectiveFov<f32>, Deg<f32>>,
    start_time: Instant,
    exposure: f32,
    gamma: f32,
    current: usize,
    lights: Vec<PointLight>,
    rng: ThreadRng,
    inital_color: (bool, [f32; 4], [f32; 4]),

    //===========//
    // App Stuff //
    //===========//
    encoder: gfx::Encoder<R, C>,

    //========//
    // Models //
    //========//
    objects: Vec<Object<R>>,
    quad: VertexSlice<R, define::V>,

    //===========//
    // Pipelines //
    //===========//
    deferred_pso: gfx::PipelineState<R, define::deferred::Meta>,
    pbr_pso: gfx::PipelineState<R, define::pbr::Meta>,
    ldr_pso: gfx::PipelineState<R, define::ldr::Meta>,
    shadow_pso: gfx::PipelineState<R, define::shadow::Meta>,

    //===============//
    // Pipeline Data //
    //===============//
    deferred_data: define::deferred::Data<R>,
    pbr_data: define::pbr::Data<R>,
    ldr_data: define::ldr::Data<R>,
    shadow_data: define::shadow::Data<R>,
}

struct Object<R: gfx::Resources> {
    pub mesh: VertexSlice<R, define::Vtnt>,
    pub sampler: Sampler<R>,
    pub normal: ShaderResourceView<R, [f32; 4]>,
    pub albedo: ShaderResourceView<R, [f32; 4]>,
    pub roughness: ShaderResourceView<R, [f32; 4]>,
    pub metalness: ShaderResourceView<R, [f32; 4]>,
}

impl<R: gfx::Resources> Object<R> {
    pub fn apply_to_data(&self, deferred: &mut define::deferred::Data<R>, pbr: &mut define::pbr::Data<R>, shadow: &mut define::shadow::Data<R>) {
        deferred.verts = self.mesh.0.clone();
        shadow.verts = self.mesh.0.clone();

        deferred.normal = (self.normal.clone(), self.sampler.clone());
        pbr.albedo = (self.albedo.clone(), self.sampler.clone());
        pbr.roughness = (self.roughness.clone(), self.sampler.clone());
        pbr.metalness = (self.metalness.clone(), self.sampler.clone());
    }
}

struct PointLight {
    pub base_angle: Deg<f32>,
    pub color: [f32; 4],
    pub ambient: [f32; 4],
}

impl PointLight {
    pub fn animate_camera(&self, time: f32) -> BasicCamera<PerspectiveFov<f32>> {
        let theta = self.base_angle + Deg(time * 30.);

        let camera = ArcBall {
            origin: Point3::new(0., 0., 0.),
            theta: theta,
            phi: Deg((theta + Deg(time * 13.)).sin() * 45.),
            dist: 7.,
            projection: PerspectiveFov {
                fovy: Deg(20.).into(),
                aspect: 1., 
                near: 0.1, far: 100.
            },
        };

        camera.to_camera()
    }
}

struct ViewPair<R: gfx::Resources, T: gfx::format::Formatted> {
    resource: gfx::handle::ShaderResourceView<R, T::View>,
    target: gfx::handle::RenderTargetView<R, T>,
}

fn build_layer<R, C, F, T>(factory: &mut F, w: texture::Size, h: texture::Size) -> ViewPair<R, T>
    where F: gfx_app::Factory<R, CommandBuffer=C>,
          R: gfx::Resources + 'static,
          C: gfx::CommandBuffer<R> + Send + 'static,
          T: format::TextureFormat,
          T::Surface: format::RenderSurface,
          T::Channel: format::RenderChannel,
{
    let (_ , srv, rtv) = factory.create_render_target(w, h).unwrap();
    ViewPair {
        resource: srv,
        target: rtv,
    }
}

fn load_image<R, C, F, T, P>(factory: &mut F, path: P) -> (Texture<R, T::Surface>, ShaderResourceView<R, T::View>)
    where F: gfx_app::Factory<R, CommandBuffer=C>,
          R: gfx::Resources + 'static,
          C: gfx::CommandBuffer<R> + Send + 'static,
          P: AsRef<Path>,
          T: format::TextureFormat,
{
    use std::io::*;
    use std::fs::File;

    let image = image::load(BufReader::new(File::open(path).expect("Image not found")), image::PNG).unwrap().to_rgba();
    let dim = image.dimensions();

    factory.create_texture_immutable_u8::<T>(
        texture::Kind::D2(dim.0 as u16, dim.1 as u16, texture::AaMode::Single),
        &[&image]
    ).expect("Could not upload texture")  // TODO: Result
}

fn get_color(mut arg: ::clap::Values) -> Result<[f32; 4], &'static str> {
    let c = arg.next().ok_or("No color provided")?;
    if c.len() != 6 { return Err("Invalid color format (not 6 chars)") }
    let z = u64::from_str_radix(c, 16).map_err(|_| "Invalid color format (not hex)")?;
    let mut rgb = [
        ((z >> 16) & 0xFF) as f32 / 255.,
        ((z >> 8 ) & 0xFF) as f32 / 255.,
        ( z        & 0xFF) as f32 / 255.,
        1.,
    ];

    if let Some(v) = arg.next() {
        rgb[3] *= v.parse().map_err(|_| "Second parameter is not a float")?
    }

    Ok(rgb)
}

fn get_args() -> (Vec<PathBuf>, usize, [f32; 4], [f32; 4]) {
    use clap::{App, Arg};

    let args = App::new("PBR Demo")
        .author(crate_authors!())
        .arg(Arg::with_name("object")
            .short("o")
            .long("objects")
            .help("list of directories, each one containing model.obj and several PBR textures")
            .required(true)
            .min_values(1))
        .arg(Arg::with_name("lights")
            .short("l")
            .long("lights")
            .help("how many point lights")
            .default_value("5"))
        .arg(Arg::with_name("ambient")
            .short("a")
            .long("ambient")
            .help("ambient color")
            .min_values(1)
            .max_values(2)
            .default_value("4d479b"))
        .arg(Arg::with_name("color")
            .short("c")
            .long("color")
            .help("light color")
            .min_values(1)
            .max_values(2)
            .default_value("e0bd91"))
    .get_matches();

    (
        args.values_of("object").unwrap().map(|v| PathBuf::from(v)).collect(),
        args.value_of("lights").map(|v| v.parse()).unwrap().expect("Could not parse light count"),
        get_color(args.values_of("ambient").unwrap()).expect("Could not parse ambient color arg"),
        get_color(args.values_of("color").unwrap()).expect("Could not parse light color arg"),
    )

}

impl<R, C> ApplicationBase<R, C> for App<R, C> where
    R: gfx::Resources + 'static,
    C: gfx::CommandBuffer<R> + Send + 'static,
{
    fn new<F>(factory: &mut F, _: gfx_app::shade::Backend, window_targets: gfx_app::WindowTargets<R>) -> Self
    where F: gfx_app::Factory<R, CommandBuffer=C>,
    {
        // read args
        let (directories, light_count, mut initial_ambient, mut initial_light) = get_args();
        // inital window size
        let dim = window_targets.color.get_dimensions();

        // load resources
        let sampler = factory.create_sampler(texture::SamplerInfo::new(
            texture::FilterMethod::Bilinear,
            texture::WrapMode::Tile,
        ));

        let objects: Vec<Object<R>> = directories.into_iter().map(|dir| {
            use self::format::*;

            Object {
                mesh: open_obj(dir.join("model.obj"), factory).unwrap(),
                normal: load_image::<_, _, _, (R8_G8_B8_A8, Unorm), _>(factory, dir.join("normal.png")).1,
                albedo: load_image::<_, _, _, (R8_G8_B8_A8, Unorm), _>(factory, dir.join("albedo.png")).1,
                metalness: load_image::<_, _, _, (R8_G8_B8_A8, Unorm), _>(factory, dir.join("metalness.png")).1,
                roughness: load_image::<_, _, _, (R8_G8_B8_A8, Unorm), _>(factory, dir.join("roughness.png")).1,
                sampler: sampler.clone(),
            }
        }).collect();

        // create shadow buffer
        let shadow_tex = {
            let kind = texture::Kind::D2(512, 512, texture::AaMode::Single);
            let bind = gfx::SHADER_RESOURCE | gfx::DEPTH_STENCIL;
            let ctype = Some(gfx::format::ChannelType::Float);

            factory.create_texture(kind, 1, bind, gfx::memory::Usage::Data, ctype).unwrap()
        };

        let shadow_tex_sampler = {
            let resource = factory.view_texture_as_shader_resource::<define::ShadowDepthFormat>(
                &shadow_tex, (0, 0), gfx::format::Swizzle::new()).unwrap();

            let mut sinfo = texture::SamplerInfo::new(
                texture::FilterMethod::Bilinear,
                texture::WrapMode::Clamp
            );

            sinfo.comparison = Some(gfx::state::Comparison::LessEqual);
            (resource, factory.create_sampler(sinfo))
        };

        let shadow_depth_target = factory.view_texture_as_depth_stencil(
            &shadow_tex, 0, None,
            texture::DepthStencilFlags::empty()).unwrap();

        // create gbuffer
        let layer_a = build_layer(factory, dim.0, dim.1);
        let layer_b = build_layer(factory, dim.0, dim.1);
        let value = build_layer(factory, dim.0, dim.1);

        let (_, _, depth) = factory.create_depth_stencil(dim.0, dim.1).unwrap();

        // create pipeline state objects
        let deferred_pso = {
            let shaders = shaders::deferred(factory).unwrap();
            factory.create_pipeline_state(
                &shaders,
                gfx::Primitive::TriangleList,
                gfx::state::Rasterizer::new_fill(),
                define::deferred::new()
            ).unwrap()
        };

        let pbr_pso = {
            let shaders = shaders::pbr(factory).unwrap();
            factory.create_pipeline_state(
                &shaders,
                gfx::Primitive::TriangleList,
                gfx::state::Rasterizer::new_fill(),
                define::pbr::new()
            ).unwrap()
        };

        let ldr_pso = {
            let shaders = shaders::ldr(factory).unwrap();
            factory.create_pipeline_state(
                &shaders,
                gfx::Primitive::TriangleList,
                gfx::state::Rasterizer::new_fill(),
                define::ldr::new()
            ).unwrap()
        };

        let shadow_pso = {
            use gfx::state::*;

            let shaders = shaders::shadow(factory).unwrap();
            factory.create_pipeline_state(
                &shaders,
                gfx::Primitive::TriangleList,
                gfx::state::Rasterizer {
                    front_face: FrontFace::CounterClockwise,
                    cull_face: CullFace::Front,
                    method: RasterMethod::Fill,
                    offset: None,
                    samples: None,
                },
                define::shadow::new()
            ).unwrap()
        };

        // create pipeline data
        let gbuf_sampler = factory.create_sampler(texture::SamplerInfo::new(
            texture::FilterMethod::Scale,
            texture::WrapMode::Clamp,
        ));

        let deferred_data = define::deferred::Data {
            verts: objects[0].mesh.0.clone(),
            transform: factory.create_constant_buffer(1),
            layer_a: layer_a.target.clone(),
            layer_b: layer_b.target.clone(),
            normal: (objects[0].normal.clone(), sampler.clone()),
            depth: depth.clone()
        };

        let quad = {
            use define::V;

            factory.create_vertex_buffer_with_slice(
                &[V { a_pos: [-1., -1., 0.] },
                  V { a_pos: [-1.,  1., 0.] },
                  V { a_pos: [ 1., -1., 0.] },
                  V { a_pos: [ 1.,  1., 0.] }],
                &[0u16, 1, 2, 3, 1, 2][..],
            )
        };

        let pbr_data = define::pbr::Data {
            verts: quad.0.clone(),
            live: factory.create_constant_buffer(1),
            light: factory.create_constant_buffer(1),
            layer_a: (layer_a.resource.clone(), gbuf_sampler.clone()),
            layer_b: (layer_b.resource.clone(), gbuf_sampler.clone()),
            albedo: (objects[0].albedo.clone(), sampler.clone()),
            metalness: (objects[0].metalness.clone(), sampler.clone()),
            roughness: (objects[0].roughness.clone(), sampler.clone()),
            shadow: shadow_tex_sampler,
            luminance: value.target.clone(),  
        };

        let ldr_data = define::ldr::Data {
            verts: quad.0.clone(),
            live: pbr_data.live.clone(),
            luminance: (value.resource.clone(), gbuf_sampler.clone()),
            color: window_targets.color.clone(),  
        };

        let shadow_data = define::shadow::Data {
            verts: objects[0].mesh.0.clone(),
            transform: factory.create_constant_buffer(1),
            depth: shadow_depth_target,
        };

        // create lights
        initial_ambient[3] *= 1.5 / light_count as f32;
        initial_light[3] *= 250. / light_count as f32;

        let inital_color = (
            true,
            initial_ambient,
            initial_light,
        );

        let lights = (0..light_count)
            .map(|i| Deg(i as f32 * 360. / light_count as f32))
            .map(|angle| PointLight {
                base_angle: angle,
                ambient: inital_color.1,
                color: inital_color.2,
            }).collect();

        // put it all together
        App {
            mouse_pos: None,
            orbit_diff: (0., 0., 0.),
            left_down: false,
            cam: ArcBall {
                origin: Point3::new(0., 0., 0.),
                theta: Deg(45.),
                phi: Deg(35.264),
                dist: 4.,
                projection: PerspectiveFov {
                    fovy: Deg(35.).into(),
                    aspect: window_targets.aspect_ratio, 
                    near: 0.1, far: 100.
                },
            },
            start_time: Instant::now(),
            exposure: 0.1,
            gamma: 2.2,
            current: 0,
            lights: lights,
            rng: thread_rng(),
            inital_color: inital_color,

            encoder: factory.create_encoder(),

            objects: objects,
            quad: quad,

            deferred_pso: deferred_pso,
            pbr_pso: pbr_pso,
            ldr_pso: ldr_pso,
            shadow_pso: shadow_pso,

            deferred_data: deferred_data,
            pbr_data: pbr_data,
            ldr_data: ldr_data,
            shadow_data: shadow_data,
        }
    }

    fn render<D>(&mut self, device: &mut D) where
        D: gfx::Device<Resources=R, CommandBuffer=C>
    {
        // camera stuff
        self.cam.theta += Deg(self.orbit_diff.0 * 0.2);
        self.cam.phi += Deg(self.orbit_diff.1 * 0.2);
        if self.cam.phi < Deg(-89.) { self.cam.phi = Deg(-89.) }
        if self.cam.phi > Deg(89.) { self.cam.phi = Deg(89.) }

        self.cam.dist *= (self.orbit_diff.2 * -0.1).exp();
        self.orbit_diff = (0., 0., 0.);

        let camera = self.cam.to_camera();

        let elapsed = self.start_time.elapsed();
        let elapsed = elapsed.as_secs() as f64 + elapsed.subsec_nanos() as f64 * 1e-9f64;

        // clear screen
        self.encoder.clear(&self.pbr_data.luminance, [0.; 4]);
        self.encoder.clear(&self.deferred_data.layer_a, [0.; 4]);
        self.encoder.clear(&self.deferred_data.layer_b, [0.; 4]);
        self.encoder.clear_depth(&self.deferred_data.depth, self.cam.projection.far);

        let model_mat = Matrix4::identity();
        let obj = &self.objects[self.current];

        self.encoder.update_constant_buffer(&self.deferred_data.transform, &define::TransformBlock {
            model: model_mat.into(),
            view: camera.get_view().into(),
            proj: camera.get_proj().into(),
        });

        self.encoder.draw(&obj.mesh.1, &self.deferred_pso, &self.deferred_data);

        self.encoder.update_constant_buffer(&self.pbr_data.live, &define::LiveBlock {
            eye_pos: camera.get_eye().to_vec().extend(1.).into(),
            exposure: self.exposure,
            gamma: self.gamma,
            time: elapsed as f32,
        });

        let lights = self.lights.iter().map(|l| (l.animate_camera(elapsed as f32), &l.color, &l.ambient));
        for (ref cam, color, ambient) in lights {
            self.encoder.update_constant_buffer(&self.shadow_data.transform, &define::TransformBlock {
                model: model_mat.into(),
                view: cam.get_view().into(),
                proj: cam.get_proj().into(),
            });
            self.encoder.clear_depth(&self.shadow_data.depth, camera.projection.far);
            self.encoder.draw(&obj.mesh.1, &self.shadow_pso, &self.shadow_data);

            self.encoder.clear_depth(&self.deferred_data.depth, self.cam.projection.far); // hack around bug
                                                                                          // TODO: fix bug

            self.encoder.update_constant_buffer(&self.pbr_data.light, &define::LightBlock {
                matrix: (cam.get_proj() * cam.get_view()).into(),
                pos: cam.get_eye().to_vec().extend(1.).into(),
                color: *color,
                ambient: *ambient,

            });
            self.encoder.draw(&self.quad.1, &self.pbr_pso, &self.pbr_data);
        }

        self.encoder.draw(&self.quad.1, &self.ldr_pso, &self.ldr_data);

        // send to GPU
        self.encoder.flush(device);
    }

    fn get_exit_key() -> Option<winit::VirtualKeyCode> {
        Some(winit::VirtualKeyCode::Escape)
    }

    fn on(&mut self, event: Event) {
        use self::Event::*;
        use winit::ElementState::*;

        match event {
            KeyboardInput(state, _, Some(code)) => {
                use winit::VirtualKeyCode::*;

                match (state, code) {
                    (Pressed, Up) => self.exposure *= 1.1,
                    (Pressed, Down) => self.exposure *= 0.9,
                    (Pressed, Right) => self.gamma *= 1.05,
                    (Pressed, Left) => self.gamma *= 0.95,
                    (Pressed, M) => {
                        self.current = (self.current + 1) % self.objects.len();
                        self.objects[self.current]
                            .apply_to_data(&mut self.deferred_data, &mut self.pbr_data, &mut self.shadow_data);
                    },
                    (Pressed, C) => {
                        let init = self.inital_color;
                        if init.0 {
                            let count = self.lights.len() as f32;

                            let ambient = vec3(self.rng.next_f32(), self.rng.next_f32(), self.rng.next_f32())
                                .normalize()
                                .extend(0.02 / count)
                                .into();

                            for l in &mut self.lights {
                                l.ambient = ambient;
                                l.color = vec3(self.rng.next_f32(), self.rng.next_f32(), self.rng.next_f32())
                                    .normalize()
                                    .extend(350. / count)
                                    .into();
                            }
                        } else {
                            for l in &mut self.lights {
                                l.ambient = init.1;
                                l.color = init.2;
                            }
                        }

                        self.inital_color.0 = !init.0;
                    },
                    _ => ()
                }
            },
            MouseMoved(x, y) => {
                let mut dx = 0;
                let mut dy = 0;

                    let p = (x, y);

                if let Some((x0, y0)) = self.mouse_pos {
                    dx = p.0 - x0;
                    dy = p.1 - y0;
                }

                self.mouse_pos = Some(p);

                if self.left_down {
                    self.orbit_diff.0 += dx as f32;
                    self.orbit_diff.1 += dy as f32;
                }
            },
            MouseLeft => self.mouse_pos = None,
            MouseInput(state, butt) => {
                use winit::MouseButton::*;

                match (state, butt) {
                    (Pressed, Left) => {
                        self.left_down = true;
                    },
                    (Released, Left) => {
                        self.left_down = false;
                    },
                    _ => (),

                }
            }
            MouseWheel(winit::MouseScrollDelta::LineDelta(_, y), _) => {
                self.orbit_diff.2 += y;
            },
            _ => (),
        }
    }

    fn on_resize<F>(&mut self, factory: &mut F, window_targets: gfx_app::WindowTargets<R>)
    where F: gfx_app::Factory<R, CommandBuffer=C>
    {
        let (w, h, _, _) = window_targets.color.get_dimensions();

        let layer_a = build_layer(factory, w, h);
        let layer_b = build_layer(factory, w, h);
        let value = build_layer(factory, w, h);

        self.deferred_data.layer_a = layer_a.target.clone();
        self.deferred_data.layer_b = layer_b.target.clone();
        self.pbr_data.luminance = value.target.clone();
        self.ldr_data.color = window_targets.color.clone();

        let (_, _, depth) = factory.create_depth_stencil(w, h).unwrap();
        self.deferred_data.depth = depth;

        self.pbr_data.layer_a.0 = layer_a.resource.clone();
        self.pbr_data.layer_b.0 = layer_b.resource.clone();
        self.ldr_data.luminance.0 = value.resource.clone();

        self.cam.projection.aspect = window_targets.aspect_ratio;
    }
}