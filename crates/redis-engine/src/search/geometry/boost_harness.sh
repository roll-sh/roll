#!/bin/sh
# Builds a local Boost.Geometry 1.88 harness that calls Boost the way RediSearch 8.10.1's
# src/geometry/rtree.cpp does, with the reference image's toolchain (Alpine 3.23, clang 21,
# libstdc++ 15, musl's libm; -O3, no NDEBUG, so Boost's assertions are on as in the module), for
# `harnessdiff.rs`. Boost (Boost Software License 1.0) is downloaded into the work directory,
# nothing is vendored. Usage:
#
#   crates/redis-engine/src/search/geometry/boost_harness.sh [workdir]   # default /tmp/roll-boost-harness
#   ROLL_GEO_HARNESS="docker exec -i roll-boost-harness /w/harness" ROLL_GEO_HSEEDS=1-1000 \
#     cargo test --release -p redis-engine --lib geometry::harnessdiff -- --ignored --nocapture
#   docker rm -f roll-boost-harness; rm -rf /tmp/roll-boost-harness                # cleanup
#
# Line protocol on stdin, one reply line per command:
#   C F|S            coordinate system (FLAT cartesian, SPHERICAL geographic<degree>)
#   V <wkt>          from_wkt + correct + is_valid: "ok" or "err <message>"
#   P <wkt>\t<wkt>   "<within(a,b)><within(b,a)><intersects(a,b)>" with the module's filters
#   N                an empty tree;  I <id> <wkt> insert;  D <id> remove ("1"/"0")
#   Q <type> <wkt>   ids in the query iterator's order (1 CONTAINS 2 WITHIN 3 DISJOINT 4 INTERSECTS)
#   M                the tree's values in order, "id:minx,miny,maxx,maxy" as the doubles' bits
# A Boost assertion (an abort in the module) answers "ASSERT <expression>".
set -eu
W=${1:-/tmp/roll-boost-harness}
NAME=${ROLL_BOOST_HARNESS_NAME:-roll-boost-harness}
mkdir -p "$W"
if [ ! -d "$W/boost_1_88_0/boost" ]; then
  curl -sSL https://archives.boost.io/release/1.88.0/source/boost_1_88_0.tar.gz -o "$W/boost.tar.gz"
  tar xzf "$W/boost.tar.gz" -C "$W" boost_1_88_0/boost
  rm "$W/boost.tar.gz"
fi
cat > "$W/harness.cpp" <<'CPP'
#define BOOST_ENABLE_ASSERT_HANDLER
#include <cstdlib>
#include <cstdio>
#include <cstring>
#include <string>
// The module is built without NDEBUG, so Boost's assertions abort the server: report them.
struct BoostAssert {
  std::string expr;
};
namespace boost {
void assertion_failed(char const* expr, char const*, char const*, long) { throw BoostAssert{expr}; }
void assertion_failed_msg(char const* expr, char const*, char const*, char const*, long) { throw BoostAssert{expr}; }
}  // namespace boost
#include <boost/geometry/geometry.hpp>
#include <boost/unordered/unordered_flat_map.hpp>
#include <cmath>
#include <iostream>
#include <string>
#include <variant>
#include <vector>

namespace bg = boost::geometry;
namespace bgm = bg::model;
namespace bgi = bg::index;

using Cartesian = bg::cs::cartesian;
using Geographic = bg::cs::geographic<bg::degree>;
using t_docId = unsigned long long;

template <typename... Ts>
struct overload : Ts... {
  using Ts::operator()...;
};
template <typename... Ts>
overload(Ts...) -> overload<Ts...>;

template <typename cs>
struct Tree {
  using point_type = bgm::point<double, 2, cs>;
  using poly_type = bgm::polygon<point_type, true, true>;
  using geom_type = std::variant<point_type, poly_type>;
  using rect_type = bgm::box<point_type>;
  using doc_type = std::pair<rect_type, t_docId>;
  using rtree_type = bgi::rtree<doc_type, bgi::quadratic<16>>;

  rtree_type rtree_;
  boost::unordered_flat_map<t_docId, geom_type> docLookup_;

  static rect_type make_mbr(geom_type const& g) {
    return std::visit(overload{[](point_type const& point) -> rect_type {
                                 constexpr auto INF = std::numeric_limits<long double>::infinity();
                                 const auto x = bg::get<0>(point);
                                 const auto y = bg::get<1>(point);
                                 const auto p1 = point_type{std::nexttoward(x, -INF), std::nexttoward(y, -INF)};
                                 const auto p2 = point_type{std::nexttoward(x, +INF), std::nexttoward(y, +INF)};
                                 return {p1, p2};
                               },
                               [](poly_type const& geom) { return bg::return_envelope<rect_type>(geom); }},
                      g);
  }

  static geom_type from_wkt(std::string const& wkt) {
    const auto geom = [&]() -> geom_type {
      if (wkt.starts_with("POI")) {
        return bg::from_wkt<point_type>(wkt);
      } else if (wkt.starts_with("POL")) {
        auto poly = bg::from_wkt<poly_type>(wkt);
        bg::correct(poly);
        return poly;
      } else {
        throw std::runtime_error{"unknown geometry type"};
      }
    }();
    std::visit(
        [](auto const& geom) -> void {
          if (std::string error; !bg::is_valid(geom, error)) {
            throw std::runtime_error{"invalid geometry: " + error};
          }
        },
        geom);
    return geom;
  }

  static bool within_f(geom_type const& a, geom_type const& b) {
    return std::visit(overload{[](auto const& geom, point_type const& point) -> bool { return false; },
                               [](auto const& geom, poly_type const& poly) -> bool { return bg::within(geom, poly); }},
                      a, b);
  }
  static bool intersects_f(geom_type const& a, geom_type const& b) {
    return std::visit([](auto const& g1, auto const& g2) -> bool { return bg::intersects(g1, g2); }, a, b);
  }

  std::vector<t_docId> query(int type, geom_type const& q) {
    auto qm = make_mbr(q);
    std::vector<t_docId> out;
    auto look = [this](doc_type const& d) -> geom_type const* {
      auto it = docLookup_.find(d.second);
      return it == docLookup_.end() ? nullptr : &it->second;
    };
    auto run = [&](auto pred) {
      for (auto it = rtree_.qbegin(pred); it != rtree_.qend(); ++it) out.push_back(it->second);
    };
    switch (type) {
      case 1:
        run(bgi::contains(qm) && bgi::satisfies([&](doc_type const& d) {
              auto g = look(d);
              return g && within_f(q, *g);
            }));
        break;
      case 2:
        run(bgi::within(qm) && bgi::satisfies([&](doc_type const& d) {
              auto g = look(d);
              return g && within_f(*g, q);
            }));
        break;
      case 3:
        run(bgi::satisfies([&](doc_type const& d) {
          if (!bg::intersects(d.first, qm)) return true;
          auto g = look(d);
          return g && !intersects_f(*g, q);
        }));
        break;
      case 4:
        run(bgi::intersects(qm) && bgi::satisfies([&](doc_type const& d) {
              auto g = look(d);
              return g && intersects_f(*g, q);
            }));
        break;
    }
    return out;
  }
};

static std::string hexd(double v) {
  unsigned long long u;
  std::memcpy(&u, &v, sizeof u);
  char b[64];
  std::snprintf(b, sizeof b, "%016llx", u);
  return b;
}

template <typename cs>
static void handle(Tree<cs>& t, char c, std::string const& rest) {
  using T = Tree<cs>;
  switch (c) {
    case 'V':
      try {
        T::from_wkt(rest);
        std::cout << "ok\n";
      } catch (std::exception const& e) {
        std::cout << "err " << e.what() << "\n";
      }
      break;
    case 'P': {
      auto tab = rest.find('\t');
      try {
        auto a = T::from_wkt(rest.substr(0, tab));
        auto b = T::from_wkt(rest.substr(tab + 1));
        bool const w1 = T::within_f(a, b), w2 = T::within_f(b, a), i = T::intersects_f(a, b);
        std::cout << w1 << w2 << i << "\n";
      } catch (std::exception const& e) {
        std::cout << "err " << e.what() << "\n";
      }
      break;
    }
    case 'N':
      t.rtree_.clear();
      t.docLookup_.clear();
      std::cout << "ok\n";
      break;
    case 'I': {
      auto sp = rest.find(' ');
      t_docId id = std::stoull(rest.substr(0, sp));
      try {
        auto g = T::from_wkt(rest.substr(sp + 1));
        t.docLookup_.insert({id, g});
        t.rtree_.insert({T::make_mbr(g), id});
        std::cout << "ok\n";
      } catch (std::exception const& e) {
        std::cout << "err " << e.what() << "\n";
      }
      break;
    }
    case 'D': {
      t_docId id = std::stoull(rest);
      auto it = t.docLookup_.find(id);
      if (it == t.docLookup_.end()) {
        std::cout << "0\n";
      } else {
        t.rtree_.remove(typename T::doc_type{T::make_mbr(it->second), id});
        t.docLookup_.erase(id);
        std::cout << "1\n";
      }
      break;
    }
    case 'Q': {
      auto sp = rest.find(' ');
      int type = std::stoi(rest.substr(0, sp));
      try {
        auto q = T::from_wkt(rest.substr(sp + 1));
        auto ids = t.query(type, q);
        std::string s;
        for (auto id : ids) s += std::to_string(id) + " ";
        std::cout << s << "\n";
      } catch (std::exception const& e) {
        std::cout << "err " << e.what() << "\n";
      }
      break;
    }
    case 'M': {
      std::string s;
      for (auto const& d : t.rtree_) {
        s += std::to_string(d.second) + ":" + hexd(bg::get<bg::min_corner, 0>(d.first)) + "," + hexd(bg::get<bg::min_corner, 1>(d.first)) + "," +
             hexd(bg::get<bg::max_corner, 0>(d.first)) + "," + hexd(bg::get<bg::max_corner, 1>(d.first)) + " ";
      }
      std::cout << s << "\n";
      break;
    }
    default:
      std::cout << "? " << c << "\n";
  }
}

int main() {
  std::ios::sync_with_stdio(false);
  Tree<Cartesian> flat;
  Tree<Geographic> sphere;
  bool sph = false;
  std::string line;
  while (std::getline(std::cin, line)) {
    if (line.empty()) continue;
    char c = line[0];
    std::string rest = line.size() > 2 ? line.substr(2) : "";
    try {
      if (c == 'C') {
        sph = rest == "S";
        std::cout << "ok\n";
      } else if (sph) {
        handle(sphere, c, rest);
      } else {
        handle(flat, c, rest);
      }
    } catch (BoostAssert const& a) {
      std::cout << "ASSERT " << a.expr << "\n";
    }
    if (c == 'Q' || c == 'M' || c == 'C' || c == 'N') std::cout.flush();
  }
  std::cout.flush();
}
CPP
docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" -v "$W:/w" alpine:3.23 sleep infinity >/dev/null
docker exec "$NAME" sh -c 'apk add --no-cache clang21 g++ musl-dev >/dev/null && cd /w &&
  clang++ -std=c++20 -O3 -fno-strict-aliasing -march=armv8-a+lse -mtune=neoverse-n1 -I boost_1_88_0 -o harness harness.cpp'
echo "ROLL_GEO_HARNESS=\"docker exec -i $NAME /w/harness\""
