use std::path::Path;
use std::time::{Duration, Instant};
use std::ops;
use std::fs::File;
use std::io::BufWriter;
use std::f64::consts;

use rand::Rng;

#[derive(Debug, Clone, Copy)]
struct Point {
  x: f64,
  y: f64,
  c: f64
}

impl Point {
  fn origin() -> Point {
    Point { x: 0.0, y: 0.0, c: 0.0 }
  }

  fn random() -> Point {
    let mut rng = rand::thread_rng();
    let x: f64 = rng.gen_range(0.0,1.0);
    let y: f64 = rng.gen_range(0.0,1.0);
    let c: f64 = rng.gen_range(0.0,1.0);
    Point { x: x, y: y, c: c }
  }

  fn theta(self) -> f64 {
    return (self.x / self.y).atan()
  }

  fn r(self) -> f64 {
    let mut r: f64 = (self.x*self.x + self.y*self.y).sqrt();
    r = 1.0/r;
    return r;
  }
}

impl ops::AddAssign<f64> for Point {
  fn add_assign(&mut self, other: f64) {
    self.x += other;
    self.y += other;
  }
}

impl ops::AddAssign for Point {
  fn add_assign(&mut self, other: Self) { 
    self.x += other.x;
    self.y += other.y;
  }
}

impl ops::MulAssign<f64> for Point {
  fn mul_assign(&mut self, other: f64) {
    self.x *= other;
    self.y *= other;
  }
}

impl ops::Mul<f64> for Point {
  type Output = Point;
  fn mul(self, other: f64) -> Point {
    Point { x: self.x * other, y: self.y * other, c: self.c * other}
  }
}

#[derive(Debug, Clone, Copy)]
struct Affine {
  a: f64,
  b: f64,
  c: f64,
  d: f64,
  e: f64,
  f: f64,
}

#[derive(Copy, Clone)]
enum NonLinearFunc {
  Linear,
  Sinus,
  Sphere,
  Swirl,
  Horse,
  Polar,
  Handkerchief,
  Heart,
  Disc,
  Spiral,
  Hyperbolic,
  Diamond,
  Ex,
  Julia,
  Bent,
  Waves,
  Fisheye,
  Popcorn,
  Exponential,
  Power,
  Cosine,
  Rings,
  Fan,
}

fn funcs() -> [NonLinearFunc; 14] {
  return [NonLinearFunc::Linear, NonLinearFunc::Sinus, NonLinearFunc::Sphere, NonLinearFunc::Swirl, NonLinearFunc::Horse, NonLinearFunc::Polar, NonLinearFunc::Handkerchief, NonLinearFunc::Heart, NonLinearFunc::Disc, NonLinearFunc::Spiral, NonLinearFunc::Hyperbolic, NonLinearFunc::Diamond, NonLinearFunc::Ex, NonLinearFunc::Julia, /*NonLinearFunc::Bent, NonLinearFunc::Waves, NonLinearFunc::Fisheye, NonLinearFunc::Popcorn, NonLinearFunc::Exponential, NonLinearFunc::Power, NonLinearFunc::Cosine, NonLinearFunc::Rings, NonLinearFunc::Fan*/];
}

const OMEGA_P: u8 = 20;
fn omega() -> f64 {
  let mut rng = rand::thread_rng();
  let p: u8 = rng.gen_range(0,255);
  if p < OMEGA_P { return consts::PI; }

  return 0.0; // this should return zero or pi
}

// applies one of the nonlinear functions to a point and returns the transformed point
fn non_linear(func_type: &NonLinearFunc, pt: &Point, base: &Affine) -> Point {
  let mut x = 0.0;
  let mut y = 0.0;
  let mut c = pt.c;
  match func_type {
    NonLinearFunc::Linear => {},
    NonLinearFunc::Sinus => {
      x = pt.x.sin();
      y = pt.y.sin();
    },
    NonLinearFunc::Sphere => {
      let r = pt.r();
      x = r*pt.x;
      y = r*pt.y;
    },
    NonLinearFunc::Swirl => {
      let r = pt.r();
      let r = r*r;
      x = pt.x*r.sin() - pt.y*r.cos();
      y = pt.x*r.cos() + pt.y*r.sin();
    },
    NonLinearFunc::Horse => {
      let r = pt.r();
      x = r * (pt.x-pt.y) * (pt.x+pt.y);
      y = r * 2.0 * pt.x * pt.y;
    },
    NonLinearFunc::Polar => {
      x = pt.theta() / consts::PI;
      y = pt.r() - 1.0;
    },
    NonLinearFunc::Handkerchief => {
      let r = pt.r();
      let theta = pt.theta();
      x = r * (theta + r).sin();
      y = r * (theta - r).cos();
    },
    NonLinearFunc::Heart => {
      let r = pt.r();
      let theta = pt.theta();
      x = r * (theta*r).sin();
      y = -r * (theta*r).cos();
    },
    NonLinearFunc::Disc => {
      let theta_pi = pt.theta() / consts::PI;
      let r_pi = pt.r() * consts::PI;
      x = theta_pi * (r_pi).sin();
      y = theta_pi * (r_pi).cos();
    }, 
    NonLinearFunc::Spiral => {
      let r = pt.r();
      let theta = pt.theta();
      x = (1.0/r) * (theta.cos()+r.sin());
      y = (1.0/r) * (theta.sin()-r.cos());
    }, 
    NonLinearFunc::Hyperbolic => {
      let theta = pt.theta();
      let r = pt.r();
      x = theta.sin()/r;
      y = r * theta.cos();
    },
    NonLinearFunc::Diamond => {
      let theta = pt.theta();
      let r = pt.r();
      x = theta.sin() * r.cos();
      y = theta.cos() * r.sin();
    },
    NonLinearFunc::Ex => {
      let theta = pt.theta();
      let r = pt.r();
      let p0 = (theta+r).sin();
      let p1 = (theta-r).cos();
      let p0 = p0*p0*p0;
      let p1 = p1*p1*p1;
      x = p0+p1;
      y = p0-p1;
    },
    NonLinearFunc::Julia => {
      let r = pt.r().sqrt();
      let theta = pt.theta() / 2.0;
      x = r * (theta+omega()).cos();
      y = r * (theta+omega()).sin();
    },
    NonLinearFunc::Bent => {
      /*if pt.x < 0.0 && pt.y >= 0.0 { x = 2.0 * pt.x; }
      else if pt.x >= 0.0 && pt.y < 0.0 { y = pt.y / 2.0; }
      else if pt.x < 0.0 && pt.y < 0.0 {
        x = 2.0 * pt.x;
        y = pt.y / 2.0; 
      }*/
    },
    NonLinearFunc::Waves => {
      /*x += base.b * (pt.y / (base.c * base.c)).sin();
      y += base.e * (pt.x / (base.f * base.f)).sin();*/
    },
    NonLinearFunc::Fisheye => {
      /*let mul = 2.0 / (pt.r() + 1.0);
      x = pt.y * mul;
      y = pt.x * mul;*/
    },
    NonLinearFunc::Popcorn => {},
    NonLinearFunc::Exponential => {},
    NonLinearFunc::Power => {},
    NonLinearFunc::Cosine => {},
    NonLinearFunc::Rings => {},
    NonLinearFunc::Fan => {},
  };
  Point {x,y,c}
}

impl Affine {
  fn identity() -> Affine {
    Affine { a: 1.0, b: 0.0, c: 0.0, d: 0.0, e: 1.0, f: 0.0 }
  }

  fn scaling(x_scale: f64, y_scale: f64) -> Affine {
    Affine { a: x_scale, e: y_scale, ..Affine::identity() }
  }

  fn apply(&self, pt: &mut Point) {
    pt.x = pt.x * self.a + pt.y * self.b + self.c;
    pt.y = pt.x * self.d + pt.y * self.e + self.f;
  }
}

struct Function {
  base: Affine,
  // tuples of nonlinearfunc type, f64 weight, and affine post transform
  variations: Vec::<(NonLinearFunc, f64, Affine)>,
  c: f64,
}

impl Function {
  // applies the function to a mutable point
  fn apply(&self, pt: &mut Point) {
    self.base.apply(pt);
    let mut pt_final = Point::origin();
    for (variation, weight, post_trans) in self.variations.iter() {
      // apply variation to point)/2.0, store in var_pt
      let mut var_pt: Point = non_linear(variation, pt, &self.base) * (*weight);
      // apply post transform for variation to pt
      //post_trans.apply(&mut var_pt); 
      // add to final point
      pt_final += var_pt;
    }
    pt.x = pt_final.x;
    pt.y = pt_final.y;
    pt.c = (pt.c + self.c)/2.0;
  }

  // generates randomized function object
  fn random() -> Function {
    let mut rng = rand::thread_rng();
    let base = Affine::identity();
    let mut variations = Vec::<(NonLinearFunc, f64, Affine)>::new();
    // weights of variations should sum to 1
    let mut weight_sum = 1.0;
    for func in &funcs() {
      let weight = rng.gen_range(0.0,weight_sum);
      weight_sum -= weight;
      variations.push((*func, weight, Affine::identity()));
    }
    let c = rng.gen_range(0.0, 1.0);
    Function { base, variations, c }
  }
}

struct IFS {
  funcs: Vec::<Function>,
  final_trans: Function,
}

impl IFS {
  // applies a single ifs round to a mutable point
  fn apply(&self, pt: &mut Point) {
    let mut rng = rand::thread_rng();
    assert!(self.funcs.len() > 0);
    let i = rng.gen_range(0,self.funcs.len());
    self.funcs[i].apply(pt);
    self.final_trans.apply(pt);
  }

  // generates an IFS with 'amt' randomized functions
  fn random(amt: i32) -> (IFS) {
    let mut funcs = Vec::<Function>::new();
    for i in (0..amt) {
      funcs.push(Function::random());
    }
    let i = IFS { funcs, final_trans: Function::random() };
    return i;
  }
}

struct Histogram {
  width: usize,
  height: usize,
  // stores frequency + rgb tuples
  data: Vec<(i64, i64, i64, i64)>,
  palette: Palette,
}

impl Histogram {
  fn new(width: usize, height: usize, palette: Palette) -> Histogram {
    let mut data = Vec::<(i64, i64, i64, i64)>::new();
    for i in 0..width*height {
      data.push((0, 0,0,0));
    }
    Histogram { width, height, data, palette }
  }

  fn add_point(&mut self, pt: Point) {
    // add 1 to frequency
    self.get_point(pt.x as usize, pt.y as usize).0 += 1;
    // assign rgb
    let color = self.palette.get_color(pt.c);
    self.get_point(pt.x as usize, pt.y as usize).1 += color.0 as i64;
    self.get_point(pt.x as usize, pt.y as usize).2 += color.1 as i64;
    self.get_point(pt.x as usize, pt.y as usize).3 += color.2 as i64;
  }

  fn get_point(&mut self, x: usize, y: usize) -> &mut (i64, i64, i64, i64) {
    return &mut (self.data[y*self.width+x]);
  }

  /* draws the histogram on the canvas
  fn draw(&mut self, canvas: &mut Canvas, trans: Affine, gamma: f64){
    for x in 0..self.width {
      for y in 0..self.height {
        let pt = self.get_point(x as usize, y as usize);
        if pt.0 == 0 {continue};    
        let scale = 1.0;//(pt.0 as f64).log2() / (pt.0 as f64);
        let a = pt.0 as f64;
        let r = pt.1 as f64 * scale;
        let g = pt.2 as f64 * scale;
        let b = pt.3 as f64 * scale;/*
        let gamma_scale = (r / 255.0).powf(1.0/gamma);
        let r = gamma_scale * 255.0;
        let gamma_scale = (g / 255.0).powf(1.0/gamma);
        let g = gamma_scale * 255.0;
        let gamma_scale = (b / 255.0).powf(1.0/gamma);
        let b = gamma_scale * 255.0;*/

        //println!("a: {}", a);
        let mut color: (u8,u8,u8,u8) = (r as u8, g as u8, b as u8, a as u8);
        let mut pt = Point{x: x as f64, y: y as f64, c: 0.0};
        trans.apply(&mut pt);
        canvas.set_pixel(pt.x as usize, pt.y as usize, color);
      }
    }
  }*/

  fn good_draw(&mut self, canvas: &mut Canvas, predown_gamma: f64, postdown_gamma: f64, do_predown_log: bool, do_predown_gamma: bool, do_postdown_log:bool, do_postdown_gamma: bool, use_alpha: bool) {
    if self.width < canvas.width || self.height < canvas.height {
      println!("cannot downsample draw with specified canvas size");
      return;
    }
    let x_sample_size = self.width / canvas.width;
    let y_sample_size = self.height / canvas.height;
    println!("downsample draw using x {} and y {}",x_sample_size,y_sample_size);
    for cx in 0..canvas.width {
      for cy in 0..canvas.height {
        let mut r: f64 = 0.0;
        let mut g: f64 = 0.0;
        let mut b: f64 = 0.0;
        let mut a: f64 = 0.0;
        let total = (x_sample_size * y_sample_size) as f64;
        for hx in cx * x_sample_size..(cx+1) * x_sample_size {
          for hy in cy * y_sample_size..(cy+1) * y_sample_size {
            // apply gamma and log
            let pt = self.get_point(hx as usize, hy as usize);
            if pt.0 == 0 {continue};
            let scale = 1.0;
            if do_predown_log {
              let scale = (pt.0 as f64).log2() / (pt.0 as f64);
            }
            let (pa,pr,pg,pb) = (pt.0 as f64, pt.1 as f64 * scale,pt.2 as f64 * scale, pt.3 as f64 * scale);
            if do_predown_gamma {
              let gamma_scale = (pr / 255.0).powf(1.0/predown_gamma);
              let pr = gamma_scale * 255.0;
              let gamma_scale = (pg / 255.0).powf(1.0/predown_gamma);
              let pg = gamma_scale * 255.0;
              let gamma_scale = (pb / 255.0).powf(1.0/predown_gamma);
              let pb = gamma_scale * 255.0;
              let gamma_scale = (pa / 255.0).powf(1.0/predown_gamma);
              let pa = gamma_scale * 255.0;
            }
            r += pr;
            g += pg;
            b += pb;
            a += pa;
          }
        }
        //println!("min a hist: {} max a hist: {}, a {}",min_a_hist,max_a_hist,a);
        if do_postdown_gamma {
          let gamma_scale = (r / 255.0).powf(1.0/postdown_gamma);
          r = gamma_scale * 255.0;
          let gamma_scale = (g / 255.0).powf(1.0/postdown_gamma);
          g = gamma_scale * 255.0;
          let gamma_scale = (b / 255.0).powf(1.0/postdown_gamma);
          b = gamma_scale * 255.0;
          let gamma_scale = (a / 255.0).powf(1.0/postdown_gamma);
          a = gamma_scale * 255.0;
        }

        r /= total;
        g /= total;
        b /= total;
        a /= total;

        if do_postdown_log {
          let scale = (a as f64).log2() / a as f64;
          r *= scale;
          g *= scale;
          b *= scale;
        }

        if r > 255.0 { r = 255.0; }
        if g > 255.0 { g = 255.0; }
        if b > 255.0 { b = 255.0; }
        if a > 255.0 { a = 255.0; }
        let mut color: (u8,u8,u8,u8) = (r as u8, g as u8, b as u8, a as u8);
        canvas.set_pixel(cx, cy, color, use_alpha);
      }
    }
  }
}

struct FractalMaker {
  hist: Histogram,
  ifs_to_hist: Affine,
  ifs: IFS,
}

impl FractalMaker {
  // iterate a point times amount of times.
  // applies ifs to the point and draws it on the histogram
  fn iterate_point(&mut self, pt: &mut Point, times: i64) {
    //println!("fractal maker iterating point");
    for iter in (0..times) {
      self.ifs.apply(pt);
      let min_render_iter = 20;
      if iter > min_render_iter {
        let mut pt_hist = Point {x: pt.x, y: pt.y, c: pt.c};
        self.ifs_to_hist.apply(&mut pt_hist);
        if pt_hist.x < self.hist.width as f64 && pt_hist.x > 0.0 && pt_hist.y < self.hist.height as f64 && pt_hist.y > 0.0 {
          self.hist.add_point(pt_hist);
        }
      }
    }
  }

  fn draw(&mut self, canvas: &mut Canvas, predown_gamma: f64, postdown_gamma: f64, do_predown_log: bool, do_predown_gamma: bool, do_postdown_log: bool, do_postdown_gamma:bool, use_alpha: bool) {
    println!("fractal maker drawing ifs to canvas");
    //self.hist.draw(canvas, hist_to_canvas, gamma);
    self.hist.good_draw(canvas, predown_gamma, postdown_gamma, do_predown_log, do_predown_gamma, do_postdown_log, do_postdown_gamma, use_alpha)
  }

  fn random(hist_width: usize, hist_height: usize, min_funcs: i32, max_funcs: i32) -> FractalMaker {
    let mut rng = rand::thread_rng();
    let c1 = rng.gen::<(u8, u8, u8)>();
    let c2 = rng.gen::<(u8, u8, u8)>();
    let c3 = rng.gen::<(u8, u8, u8)>();
    let pal = Palette::gradient3(c1,c2,c3);
    let hist = Histogram::new(hist_width, hist_height, pal);

    let ifs_to_hist = Affine { c: 0.5*hist_width as f64, f: 0.5*hist_height as f64, ..Affine::scaling(hist_width as f64 * 0.5, hist_height as f64 * 0.5)};

    let mut rng = rand::thread_rng(); 
    let num_funcs: i32 = rng.gen_range(min_funcs,max_funcs);
    let ifs = IFS::random(num_funcs);

    FractalMaker{ hist:hist, ifs_to_hist:ifs_to_hist, ifs:ifs/*, pts:pts*/ }
  }
}


struct Canvas {
  width: usize,
  height: usize,
  pixels: Vec<u8>,
  unit_to_canvas: Affine
}

impl Canvas {
  fn new(width: usize, height: usize) -> Canvas {
    let mut pixels = Vec::<u8>::new();
    for i in (0..width*height*4) {
      pixels.push(0);
    }
    let unit_to_canvas = Affine::scaling(width as f64 - 1.0, height as f64 - 1.0);
    Canvas { width: width, height: height, pixels: pixels, unit_to_canvas: unit_to_canvas }
  }

  fn set_pixel(&mut self, x: usize, y: usize, color: (u8,u8,u8,u8), use_alpha:bool) {
    if (x > self.width-1 || y > self.height-1) {
      println!("ERROR: TRIED TO SET PIXEL OUTSIDE OF WIDTH OR HEIGHT {} {}", x, y);
      return;
    }
    let idx: usize = (4*y) as usize *self.width + (4*x) as usize;
    self.pixels[idx] = color.0;
    self.pixels[idx+1] = color.1;
    self.pixels[idx+2] = color.2;
    if use_alpha {
      self.pixels[idx+3] = color.3;
    } else {
      self.pixels[idx+3] = 255;//color.3;
    }
  }

  fn save_to_file(&self, file: &str) {
    let path = Path::new(file);
    let file = File::create(path).unwrap();
    let ref mut w = BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, self.width as u32, self.height as u32); 
    encoder.set_color(png::ColorType::RGBA);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&self.pixels[0..]).unwrap();
  }
}

struct Palette {
  colors: [(u8,u8,u8); 256]
}

impl Palette {
  fn get_color(&self, c: f64) -> (u8,u8,u8) {
    if c < 0.0 || c > 1.0 {
      println!("invalid color coord c is {}",c);
    }
    return self.colors[((c*255.0).round()) as usize]
  }

  fn gradient(start: (u8,u8,u8), end: (u8,u8,u8)) -> Palette {
    println!("generating gradient from {},{},{} to {},{},{}", start.0,start.1,start.2, end.0,end.1,end.2);
    let r: f64 = start.0 as f64;
    let g: f64 = start.1 as f64;
    let b: f64 = start.2 as f64;
    // color delta
    let dr: f64 = end.0 as f64 - start.0 as f64;
    let dr = dr/256.0;
    let dg: f64 = end.1 as f64 - start.1 as f64;
    let dg = dg/256.0;
    let db: f64 = end.2 as f64 - start.2 as f64;
    let mut colors = [start; 256];
    let mut current: (f64,f64,f64) = (start.0 as f64, start.1 as f64, start.2 as f64);
    for i in (0..255) {
      colors[i] = (current.0.round() as u8, current.1.round() as u8, current.2.round() as u8);
      current.0 += dr;
      current.1 += dg;
      current.2 += db;
    }
    Palette { colors: colors }
  }

  fn gradient3(start: (u8,u8,u8), mid: (u8,u8,u8), end: (u8,u8,u8)) -> Palette {
    println!("generating gradient from {},{},{} to {},{},{}", start.0,start.1,start.2, end.0,end.1,end.2);
    let p1 = Palette::gradient(start,mid);
    let p2 = Palette::gradient(mid,end);
    let mut colors = [start; 256];
    for i in (0..127) {
      colors[i] = p1.colors[i*2];
      colors[i+127] = p2.colors[i*2];
    }
    Palette { colors: colors }
  }
}

fn main() {
  /*let mut p: Point = Point::random();
  println!("Pre identity transform: {:?}", p);
  let id: Affine = Affine::identity();
  id.apply(&mut p);
  println!("Post identity transform: {:?}", p);
  println!("Post sinus: {:?}", p);*/
  
  //c.set_pixel(0 as usize, 0 as usize, 255,0,0,255);
  //c.set_pixel(99 as usize, 99 as usize, 255,0,0,255);
  let now = Instant::now();
  let min_funcs = 2;
  let max_funcs = 10;
  let mut fm = FractalMaker::random(16000 as usize, 16000 as usize, min_funcs, max_funcs);
  println!("iterating points");
  let mut pt = Point::random();
  fm.iterate_point(&mut pt, 32000000);
  let mut c: Canvas = Canvas::new(4000 as usize, 4000 as usize);
  println!("drawing fractal to histogram, {} sec elapsed", now.elapsed().as_secs());
  let do_predown_log = false;
  let do_predown_gamma = false;
  let do_postdown_log = false;
  let do_postdown_gamma = true;
  let use_alpha = false;
  let predown_gamma = 2.2;
  let postdown_gamma = 2.2;
  fm.draw(&mut c, predown_gamma, postdown_gamma, do_predown_log, do_predown_gamma, do_postdown_log, do_postdown_gamma, use_alpha);
  println!("saving fractal, {} sec elapsed", now.elapsed().as_secs());
  c.save_to_file("test.png");

  /*let do_predown_log = false;
  let do_predown_gamma = false;
  let do_postdown_log = false;
  let do_postdown_gamma = true;
  let predown_gamma = 2.2;
  let postdown_gamma = 2.2;
  let mut c: Canvas = Canvas::new(1000 as usize, 1000 as usize);
  fm.draw(&mut c, predown_gamma, postdown_gamma, do_predown_log, do_predown_gamma, do_postdown_log, do_postdown_gamma);
  println!("saving fractal, {} sec elapsed", now.elapsed().as_secs());
  c.save_to_file("test2.png");*/
}
