-- Favoriete ritten: a saved route is either one way (enkele reis) or a round
-- trip (heen en terug). Picking a round-trip favourite in the trip form ticks
-- "heen en terug", so the return leg is saved straight after the first one.
-- `km` stays the distance of one leg.
ALTER TABLE saved_routes ADD COLUMN round_trip INTEGER NOT NULL DEFAULT 0
    CHECK (round_trip IN (0, 1));
