#!/usr/bin/env python3
"""Compare terrio's PROJJSON export (`SpatialRef::to_projjson`) with live PROJ and GDAL.

52 CRSs are compared with PROJ's PROJJSON by meaning (types, identifiers, datums, ellipsoids,
prime meridians, axes, methods and parameters by EPSG code in SI units) and read by GDAL, whose
transformation of points (through each CRS's geographic CRS, a vertical CRS paired with WGS 84,
no grids) must match the original's within 1e-8 degrees and 1 mm; with no point the original
transforms, a case is `not compared`, and a point only the original transforms differs. Every IAU_2015 CRS is exported and read by GDAL the same way, reported
per body; the only refusals allowed there are projections without an EPSG method and projections
on an auxiliary sphere of an ellipsoid.
"""
import argparse
import json
import math
import os
import sqlite3
import subprocess
import tempfile
from collections import defaultdict
from pathlib import Path

from projjson_reference_env import no_grid_proj

from osgeo import osr

ROOT = Path(__file__).resolve().parents[1]
DEG = math.pi / 180
DEFINITIONS = [
    # Geographic 2D, 3D and geocentric.
    'EPSG:4326', 'EPSG:4258', 'EPSG:4269', 'EPSG:4267', 'EPSG:4277', 'EPSG:4807', 'EPSG:4230', 'EPSG:4283', 'EPSG:4148',
    'EPSG:4979', 'EPSG:4937', 'EPSG:4978', 'EPSG:4936',
    # Projected: one or more CRSs per method, feet, grads, axis orders.
    'EPSG:32632', 'EPSG:32761', 'EPSG:27700', 'EPSG:2263', 'EPSG:3857', 'EPSG:2154', 'EPSG:28992', 'EPSG:3035', 'EPSG:31467',
    'EPSG:27572', 'EPSG:2056', 'EPSG:21781', 'EPSG:3031', 'EPSG:5070', 'EPSG:3395', 'EPSG:2193', 'EPSG:6933', 'EPSG:3078',
    'EPSG:29873', 'EPSG:5514', 'EPSG:2065', 'EPSG:4087', 'EPSG:3034', 'EPSG:3112',
    # Vertical and compound.
    'EPSG:5773', 'EPSG:5703', 'EPSG:3855', 'EPSG:9707', 'EPSG:7415', 'EPSG:4326+5773',
    # Bound.
    '+proj=utm +zone=33 +ellps=intl +towgs84=-87,-98,-121',
    '+proj=longlat +ellps=bessel +towgs84=598.1,73.7,418.2,0.202,0.045,-2.455,6.7',
    # Reviewed cases (#311): LCC's method and origin, eqc's and cea's lat_0, Krovak's defaults, a
    # WKT with its code and TOWGS84.
    '+proj=lcc +lat_0=30 +lat_1=40 +lat_2=40 +ellps=WGS84', '+proj=lcc +lat_1=45 +lat_0=40 +ellps=WGS84',
    '+proj=lcc +lat_1=33 +k_0=0.99 +ellps=WGS84', '+proj=eqc +lat_0=10 +lat_ts=30 +ellps=WGS84',
    '+proj=cea +lat_0=10 +lat_ts=30 +ellps=WGS84', '+proj=krovak +ellps=bessel',
    'PROJCS["DHDN / 3-degree Gauss-Kruger zone 3",GEOGCS["DHDN",DATUM["Deutsches_Hauptdreiecksnetz",SPHEROID["Bessel 1841",'
    '6377397.155,299.1528128],TOWGS84[598.1,73.7,418.2,0.202,0.045,-2.455,6.7]],PRIMEM["Greenwich",0],UNIT["degree",'
    '0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",9],'
    'PARAMETER["scale_factor",1],PARAMETER["false_easting",3500000],PARAMETER["false_northing",0],UNIT["metre",1],'
    'AUTHORITY["EPSG","31467"]]',
]
# What terrio's PROJJSON would describe wrongly, so must refuse, and the reason it must give:
# auxiliary spheres of an ellipsoid named by a datum or by default, a +towgs84 of 6 values,
# parameters it does not write, an unknown prime meridian, Krovak with another cone axis.
REFUSED = {
    '+proj=laea +R_A +lat_0=52 +lon_0=10 +datum=WGS84': 'a projection on an auxiliary sphere (+R_A)',
    '+proj=laea +R_A +lat_0=52 +lon_0=10': 'a projection on an auxiliary sphere (+R_A)',
    '+proj=longlat +ellps=bessel +towgs84=1,2,3,4,5,6': 'a +towgs84 of 6 values',
    '+proj=cea +k_0=0.5 +ellps=WGS84': 'the parameter +k_0=0.5',
    '+proj=longlat +lon_0=10 +datum=WGS84': 'the parameter +lon_0=10',
    '+proj=longlat +datum=WGS84 +pm=foo': 'the prime meridian +pm=foo',
    '+proj=krovak +alpha=31 +ellps=bessel': 'a Krovak projection with +alpha=31',
}
ALLOWED_REFUSALS = ('no PROJJSON for the projection', 'no PROJJSON for a projection on an auxiliary sphere')


def srs(text):
    s = osr.SpatialReference()
    s.SetFromUserInput(text)
    s.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    return s


def factor(unit):
    if isinstance(unit, dict):
        return unit['conversion_factor']
    return {'degree': DEG, 'grad': math.pi / 200, 'arc-second': DEG / 3600, 'parts per million': 1e-6}.get(unit, 1.0)


def si(v, default):
    return v['value'] * factor(v.get('unit')) if isinstance(v, dict) else v * default


def datum_name(d):
    """A datum's name, normalized: case, GDAL's underscores, PROJ's names of unnamed datums and
    the WGS 84 and ETRS89 ensembles (written as their datum, as documented)."""
    name = d.get('name', 'unknown').lower().replace('_', ' ').removesuffix(' ensemble')
    return 'unknown' if name.startswith('unknown based on') else name


def canonical(v):
    """What the comparison looks at (as PROJ and terrio may name and identify inner objects)."""
    out = {'type': v.get('type'), 'name': None if 'id' in v else v.get('name'), 'id': v.get('id', {}).get('code')}
    d = v.get('datum') or v.get('datum_ensemble')
    if d and 'ellipsoid' in d:  # not vertical datums
        e = d['ellipsoid']
        a = si(e['semi_major_axis'] if 'semi_major_axis' in e else e['radius'], 1)
        rf = e.get('inverse_flattening') or (a / (a - si(e['semi_minor_axis'], 1)) if 'semi_minor_axis' in e else 0.0)
        pm = si(d['prime_meridian']['longitude'], DEG) if 'prime_meridian' in d else 0.0
        out['datum'] = [datum_name(d), round(a, 3), round(rf, 6), round(pm, 11)]
    if 'coordinate_system' in v:
        axes = [[a['direction'], round(factor(a.get('unit')), 15)] for a in v['coordinate_system']['axis']]
        if any('meridian' in a for a in v['coordinate_system']['axis']):  # polar CSs: east and north in terrio
            axes[0][0], axes[1][0] = 'east', 'north'
        out['cs'] = [v['coordinate_system']['subtype'], axes]
    for key in ('conversion', 'transformation'):
        if key in v:
            c = v[key]
            params = sorted((p['id']['code'], round(si({'value': p['value'], 'unit': p.get('unit')}, 1), 9)) for p in c['parameters'])
            out[key] = [c['method'].get('id', {}).get('code'), params]
    for key in ('base_crs', 'source_crs', 'target_crs'):
        if key in v:
            out[key] = {k: x for k, x in canonical(v[key]).items() if k not in ('name', 'cs')}
    if 'components' in v:
        out['components'] = [dict(canonical(c), **({'id': None, 'name': None} if 'id' in v else {})) for c in v['components']]
    return out


def export(definitions, timeout):
    """terrio's PROJJSON (or error) of each definition, through the `export_report` test (built
    and run within `timeout` seconds)."""
    with tempfile.TemporaryDirectory() as tmp:
        cases, report = Path(tmp) / 'cases.txt', Path(tmp) / 'report.jsonl'
        cases.write_text('\n'.join(definitions) + '\n')
        env = dict(os.environ, TERRIO_PROJJSON_EXPORT_CASES=str(cases), TERRIO_PROJJSON_EXPORT_REPORT=str(report))
        try:
            run = subprocess.run(['cargo', 'test', '-q', '--release', '-p', 'terrio-crs', '--test', 'projjson_export', 'export_report'],
                                 cwd=ROOT, env=env, capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired as error:
            raise RuntimeError(f'cargo test did not finish within {timeout} s (--timeout)') from error
        if run.returncode:
            raise RuntimeError(f'cargo test failed:\n{run.stdout[-2000:]}\n{run.stderr[-2000:]}')
        return {r['definition']: r for r in map(json.loads, report.read_text().splitlines())}


def difference(ours, original, base, points):
    """The largest differences, in degrees of `base` and in metres of height, between transforming
    the `points` (in `base`) that are in the original CRS's domain from `ours` and from `original`;
    `None` when none is. A point the original transforms and `ours` does not differs infinitely."""
    found = None
    for p in points:
        try:
            q = osr.CoordinateTransformation(base, original).TransformPoint(*p, 0.0)
            a = osr.CoordinateTransformation(original, base).TransformPoint(*q)
        except RuntimeError:
            continue
        try:
            b = osr.CoordinateTransformation(ours, base).TransformPoint(*q)
        except RuntimeError:
            return (math.inf, math.inf)
        d = (max(abs(a[0] - b[0]), abs(a[1] - b[1])), abs(a[2] - b[2]))
        found = d if found is None else (max(found[0], d[0]), max(found[1], d[1]))
    return found


def status(d):
    """The status of `difference()`'s result: within 1e-8 degrees and 1 mm; `not compared` when
    no point is in the original CRS's domain."""
    if d is None:
        return 'not compared'
    return 'identical' if d == (0, 0) else 'within tolerance' if d[0] <= 1e-8 and d[1] <= 1e-3 else 'differs'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--json', action='store_true')
    parser.add_argument('--timeout', type=float, default=7200, help='seconds for building and running the export (default 7200)')
    args = parser.parse_args()
    osr.UseExceptions()
    database = next(Path(p) / 'proj.db' for p in osr.GetPROJSearchPaths() if (Path(p) / 'proj.db').is_file())
    with sqlite3.connect(f'file:{database}?mode=ro', uri=True) as db:
        iau = [f'IAU_2015:{c}' for (c,) in db.execute("SELECT code FROM crs_view WHERE auth_name = 'IAU_2015' ORDER BY CAST(code AS INTEGER)")]
    ours = export(DEFINITIONS + list(REFUSED) + iau, args.timeout)
    results = []
    for definition in DEFINITIONS:
        r = ours[definition]
        try:
            proj = json.loads(srs(definition + (' +type=crs' if definition.startswith('+') else '')).ExportToPROJJSON())
            if 'error' in r:
                raise ValueError(r['error'])
            if canonical(r['projjson']) != canonical(proj):
                raise ValueError(f"PROJ {canonical(proj)}\nterrio {canonical(r['projjson'])}")
            original, back = srs(definition), srs(json.dumps(r['projjson']))
            if original.IsVertical() and not original.IsCompound():
                # No coordinates of its own: both paired with WGS 84, heights through EPSG:4979.
                wgs84 = srs('EPSG:4326')
                pair = []
                for vertical in (original, back):
                    c = osr.SpatialReference()
                    c.SetCompoundCS('WGS 84 + vertical', wgs84, vertical)
                    c.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
                    pair.append(c)
                original, back = pair[0], pair[1]
            area = original.GetAreaOfUse()
            point = ((area.west_lon_degree + area.east_lon_degree) / 2, (area.south_lat_degree + area.north_lat_degree) / 2) if area else (10.0, 50.0)
            # Through the CRS's own geographic CRS (geocentric, compound and bound CRSs too).
            base = original.CloneGeogCS()
            base.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
            d = difference(back, original, base, [point, (10.0, 50.0)])
            detail = 'no point the original transforms' if d is None else f'{d[0]:.3g} degrees, {d[1]:.3g} m' if d != (0, 0) else ''
            results.append(dict(group='CRSs (PROJ by meaning, GDAL by coordinates)', name=definition, status=status(d), detail=detail))
        except Exception as error:
            results.append(dict(group='CRSs (PROJ by meaning, GDAL by coordinates)', name=definition, status='differs', detail=str(error)))
    for definition, reason in REFUSED.items():
        r = ours[definition]
        # Refused for its reason, not for any error (a definition terrio cannot parse).
        ok = 'error' in r and f'no PROJJSON for {reason}' in r['error']
        results.append(dict(group='Refused (would be described wrongly)', name=definition, status='identical' if ok else 'differs',
                            detail=r.get('error', 'written') if not ok else ''))
    bodies = defaultdict(lambda: dict(written=0, refused=0, worst=0.0, bad=[], unchecked=[]))
    points = [(10, 20), (0, 85), (0, -85), (90, 60), (-90, -60), (180, 0), (45, 45)]
    for code in iau:
        body = bodies[int(code.split(':')[1]) // 100]
        r = ours[code]
        if 'error' in r:
            body['refused'] += 1
            # The message after the error kind's prefix (`coordinate reference system: `).
            if not r['error'].split(': ', 1)[-1].startswith(ALLOWED_REFUSALS):
                body['bad'].append(f"{code}: {r['error']}")
            continue
        body['written'] += 1
        base = srs(f"IAU_2015:{int(code.split(':')[1]) // 100 * 100}")
        d = difference(srs(json.dumps(r['projjson'])), srs(code), base, points)
        if d is None:
            body['unchecked'].append(code)
        elif status(d) == 'differs':
            body['bad'].append(f'{code}: {d}')
        body['worst'] = max(body['worst'], d[0] if d else 0.0)
    for n, b in sorted(bodies.items()):
        name = f'IAU_2015:{n * 100}..: {b["written"]} written, {b["refused"]} refused'
        state = 'differs' if b['bad'] else 'not compared' if b['unchecked'] else status((b['worst'], 0))
        detail = '; '.join(b['bad'][:3]) or (f"no point the original transforms: {', '.join(b['unchecked'][:3])}" if b['unchecked'] else '')
        results.append(dict(group='IAU_2015 (GDAL by coordinates)', name=name, status=state, detail=detail or (f'{b["worst"]:.3g} degrees' if b['worst'] else '')))
    print(json.dumps(results, indent=2) if args.json else '\n'.join(f'{r["name"]}: {r["status"]} {r["detail"]}' for r in results))
    return int(any(r['status'] == 'differs' for r in results))


if __name__ == '__main__':
    with no_grid_proj():
        raise SystemExit(main())
