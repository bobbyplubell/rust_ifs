use std::path::Path;
use std::ops;
use std::fs::File;
use std::io::BufWriter;

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
  Horse
}

fn funcs() -> [NonLinearFunc; 5] {
  return [NonLinearFunc::Linear, NonLinearFunc::Sinus, NonLinearFunc::Sphere, NonLinearFunc::Swirl, NonLinearFunc::Horse];
}

// applies one of the nonlinear functions to a point and returns the transformed point
fn non_linear(func_type: &NonLinearFunc, pt: &Point) -> Point {
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
      let r: f64 = (pt.x*pt.x + pt.y*pt.y).sqrt();
      let r = 1.0/(r*r);
      x = r*pt.x;
      y = r*pt.y;
    },
    NonLinearFunc::Swirl => {
      let r: f64 = (pt.x*pt.x + pt.y*pt.y).sqrt();
      let r = (r*r);
      x = pt.x*r.sin() - pt.y*r.cos();
      y = pt.x*r.cos() + pt.y*r.sin();
    },
    NonLinearFunc::Horse => {
      let r: f64 = (pt.x*pt.x + pt.y*pt.y).sqrt();
      let r = 1.0/r;
      x = r * (pt.x-pt.y) * (pt.x+pt.y);
      y = r * 2.0 * pt.x * pt.y;
    },
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
      let mut var_pt: Point = non_linear(variation, pt) * (*weight);
      // apply post transform for variation to pt
      post_trans.apply(&mut var_pt); 
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
  final_trans: Affine,
}

impl IFS {
  // applies a single ifs round to a mutable point
  fn apply(&self, pt: &mut Point) {
    let mut rng = rand::thread_rng();
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
    let i = IFS { funcs, final_trans: Affine::identity() };
    return i;
  }
}

struct Histogram {
  width: usize,
  height: usize,
  // stores frequency + rgb tuples
  data: Vec<(i64, f64)>
}

impl Histogram {
  fn new(width: usize, height: usize) -> Histogram {
    let mut data = Vec::<(i64, f64)>::new();
    for i in 0..width*height {
      data.push((0, 0.0));
    }
    Histogram { width, height, data }
  }

  fn add_point(&mut self, pt: Point) {
    let &mut hist_pt = self.get_point(pt.x as usize, pt.y as usize); 
    // add 1 to frequency
    hist_pt.0 += 1;
    // assign rgb
    hist_pt.1 += pt.c;
  }

  fn get_point(&mut self, x: usize, y: usize) -> &mut (i64, f64) {
    return &mut (self.data[y*self.width+x]);
  }

  // draws the histogram on the canvas
  fn draw(canvas: Canvas, palette: Palette){}
}

struct FractalMaker {
  hist: Histogram,
  palette: Palette,
  ifs: IFS,
  pts: Vec::<Point>
}

impl FractalMaker {
  // iterate a point times amount of times.
  // applies ifs to the point and draws it on the histogram
  fn iterate_point(pt: &mut Point, times: i64) {
    
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

  fn set_pixel(&mut self, x: usize, y: usize, color: (u8,u8,u8,u8)) {
    if (x > self.width-1 || y > self.height-1) {
      println!("ERROR: TRIED TO SET PIXEL OUTSIDE OF WIDTH OR HEIGHT {} {}", x, y);
      return;
    }
    let idx: usize = (4*y) as usize *self.width + (4*x) as usize;
    self.pixels[idx] = color.0;
    self.pixels[idx+1] = color.1;
    self.pixels[idx+2] = color.2;
    self.pixels[idx+3] = color.3;
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
    return self.colors[(c*255.0).round() as usize]
  }

  fn gradient(start: (u8,u8,u8), end: (u8,u8,u8)) -> Palette {
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
}

fn main() {
  let mut p: Point = Point {x: 0.0, y: 1.5, c: 1.0};
  println!("Pre transform: {:?}", p);
  let id: Affine = Affine::identity();
  id.apply(&mut p);
  println!("Post transform: {:?}", p);
  println!("Post sinus: {:?}", p);
  let mut c: Canvas = Canvas::new(100 as usize, 100 as usize);
  /*for x in (20..80) {
    for y in (20..80) {
      c.set_pixel(x as usize, y as usize, 255,0,0,255);
    }
  }*/
  //c.set_pixel(0 as usize, 0 as usize, 255,0,0,255);
  //c.set_pixel(99 as usize, 99 as usize, 255,0,0,255);
  let pal = Palette::gradient((255,0,0),(0,255,0));
  c.save_to_file("test.png");
}
